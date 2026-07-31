//! The review engine: ground → ask each lens in parallel → corroborate → merge
//! votes → rank (spec 16).
//!
//! Nothing here writes to the workspace, and there is no seam through which it
//! could: the engine takes `&Path` to *read* the repo for grounding and returns
//! findings. **Review never rewrites code.**

use std::path::Path;
use std::sync::Mutex;

use sc_model::ModelBackend;

use crate::corroborate::{self, Context};
use crate::diff::IntegratedDiff;
use crate::finding::{Finding, Lens, ModelId, Severity};
use crate::ground::{ground, Grounding};
use crate::lens::run_lens;
use crate::rank;

/// What to do with a finding, in increasing order of intervention (spec 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    /// Findings ride along with the report and the event stream; the run still
    /// succeeds. The honest default, because an uncorroborated finding is a
    /// suggestion and a suggestion that halts a run is a tool that gets switched
    /// off.
    #[default]
    Report,
    /// A corroborated finding at or above the gating severity stops the run for a
    /// human checkpoint.
    Gate,
    /// A corroborated finding becomes feedback on a re-dispatch of the same
    /// subtask, exactly as still-failing tests do. The highest-value outcome, and
    /// the reason the spec is worth building.
    Retry,
}

/// How the review runs.
#[derive(Debug, Clone)]
pub struct ReviewConfig {
    /// **Off by default.** A flag turns it on (spec 16 — "Cost, and when it
    /// doesn't run").
    pub enabled: bool,
    /// Which questions to ask. Defaults to all four.
    pub lenses: Vec<Lens>,
    /// Skip a diff smaller than this many changed lines: a three-line change does
    /// not need four lenses.
    pub min_changed_lines: usize,
    /// What happens to a finding.
    pub action: Action,
    /// The severity at which a *corroborated* finding stops the run.
    pub gate_at: Severity,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lenses: Lens::ALL.to_vec(),
            min_changed_lines: 10,
            action: Action::Report,
            gate_at: Severity::High,
        }
    }
}

/// One reviewer: an identity plus the backend to reach it through. The panel is a
/// list of these — for now always of length one, but the type does not know that
/// and nothing downstream assumes it (spec 16 — the panel is a follow-up).
pub struct Reviewer<'a> {
    pub id: ModelId,
    pub backend: &'a (dyn ModelBackend + Sync),
}

impl<'a> Reviewer<'a> {
    pub fn new(id: impl Into<String>, backend: &'a (dyn ModelBackend + Sync)) -> Self {
        Self {
            id: ModelId::new(id),
            backend,
        }
    }
}

/// The outcome of reviewing one integrated diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// Findings, ranked. Empty is the normal case.
    pub findings: Vec<Finding>,
    /// Reviewers that could not be reached. Carried explicitly rather than
    /// inferred from a shorter `considered_by`, so a renderer can say "3 of 4
    /// reviewers ran" instead of quietly reporting a narrower review as a
    /// complete one.
    pub reviewers_skipped: Vec<ModelId>,
    /// The review did not run at all — the diff was below the size threshold, or
    /// there was nothing to review.
    pub skipped: bool,
}

impl ReviewOutcome {
    /// A review that did not run. Distinct from one that ran and found nothing.
    pub fn skipped() -> Self {
        Self {
            skipped: true,
            ..Default::default()
        }
    }

    /// The findings that met the bar to stop the run: corroborated **and** at or
    /// above `gate_at`.
    pub fn blocking(&self, gate_at: Severity) -> usize {
        rank::blocking(&self.findings, gate_at).len()
    }

    /// The retry feedback these findings justify — evidence only, `None` when
    /// nothing is corroborated.
    pub fn retry_feedback(&self) -> Option<String> {
        rank::retry_feedback(&self.findings)
    }
}

/// Review one integrated diff.
///
/// `workspace` is the real workspace, already holding the integrated change —
/// read for grounding, never written. `subtask_files` is the subtask's declared
/// file list, which only the unrelated-changes check consults (and which is
/// routinely empty, hence `Unknown` rather than a pass).
pub fn review(
    reviewers: &[Reviewer<'_>],
    diff: &IntegratedDiff,
    workspace: &Path,
    goal: &str,
    subtask_files: &[String],
    cfg: &ReviewConfig,
) -> ReviewOutcome {
    if !cfg.enabled || diff.is_empty() || cfg.lenses.is_empty() || reviewers.is_empty() {
        return ReviewOutcome::skipped();
    }
    if diff.changed_lines() < cfg.min_changed_lines {
        return ReviewOutcome::skipped();
    }

    // Retrieve first, then ask. Once per review, shared by every lens and every
    // reviewer — the index is not re-walked per call.
    let grounding = ground(workspace, diff);

    let (raw, skipped) = ask_everyone(reviewers, diff, &grounding, goal, cfg);

    // Corroborate BEFORE merging: a check speaks to one claim, and merging first
    // would let one reviewer's corroborated finding lend its evidence to another's
    // differently-anchored claim.
    let ctx = Context {
        diff,
        grounding: &grounding,
        subtask_files,
    };
    let mut findings = raw;
    for f in &mut findings {
        corroborate::apply(f, &ctx);
        f.anchor_unresolved = !anchor_resolves(workspace, f);
    }

    let considered: Vec<ModelId> = reviewers
        .iter()
        .map(|r| r.id.clone())
        .filter(|id| !skipped.contains(id))
        .collect();
    let mut findings = rank::merge_votes(findings, &considered);
    rank::rank(&mut findings);

    ReviewOutcome {
        findings,
        reviewers_skipped: skipped,
        skipped: false,
    }
}

/// Run every (reviewer × lens) call in parallel, collecting findings and the
/// reviewers that could not be reached.
///
/// A reviewer whose *every* call failed is skipped — its absence recorded, the
/// remaining reviewers still reporting. A review that failed closed on a network
/// error would make the whole gate hostage to an API outage.
fn ask_everyone(
    reviewers: &[Reviewer<'_>],
    diff: &IntegratedDiff,
    grounding: &Grounding,
    goal: &str,
    cfg: &ReviewConfig,
) -> (Vec<Finding>, Vec<ModelId>) {
    let findings: Mutex<Vec<Finding>> = Mutex::new(Vec::new());
    // (reviewer, lens) → did the call reach the backend at all?
    let reached: Mutex<Vec<(ModelId, bool)>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for reviewer in reviewers {
            for lens in &cfg.lenses {
                let (findings, reached) = (&findings, &reached);
                let lens = *lens;
                scope.spawn(move || {
                    match run_lens(reviewer.backend, &reviewer.id, lens, diff, grounding, goal) {
                        Ok(found) => {
                            reached.lock().unwrap().push((reviewer.id.clone(), true));
                            findings.lock().unwrap().extend(found);
                        }
                        Err(_) => reached.lock().unwrap().push((reviewer.id.clone(), false)),
                    }
                });
            }
        }
    });

    let reached = reached.into_inner().unwrap();
    let skipped: Vec<ModelId> = reviewers
        .iter()
        .map(|r| r.id.clone())
        .filter(|id| !reached.iter().any(|(who, ok)| who == id && *ok))
        .collect();

    // Threads finish in nondeterministic order; sort so two runs over the same
    // scripted inputs produce the same list (spec 03 — determinism).
    let mut findings = findings.into_inner().unwrap();
    findings.sort_by(|a, b| {
        a.lens
            .cmp(&b.lens)
            .then(a.anchor.file.cmp(&b.anchor.file))
            .then(a.anchor.hunk.cmp(&b.anchor.hunk))
            .then(a.raised_by.cmp(&b.raised_by))
            .then(a.summary.cmp(&b.summary))
    });
    (findings, skipped)
}

/// Does the symbol a finding named actually exist in the file it named? A cheap
/// hallucination check: the index has the answer, and a model that cited a symbol
/// that isn't there got the anchor wrong.
///
/// A finding that names no symbol is not unresolved — it claimed nothing to
/// check. Only a *named* symbol can fail to resolve.
fn anchor_resolves(workspace: &Path, finding: &Finding) -> bool {
    let Some(symbol) = &finding.anchor.symbol else {
        return true;
    };
    sc_index::find_symbol_hits(workspace, symbol)
        .iter()
        .any(|hit| hit.path == finding.anchor.file)
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
