//! The swarm's side of post-integration review (spec 16): run the engine over an
//! integrated diff, emit its events, and turn its findings into a decision.
//!
//! The engine in `sc-review` knows nothing about subtasks or retries. This module
//! is the join, and it holds the two rules the orchestrator must not get wrong:
//!
//! * **Only a corroborated finding may feed a retry** — [`retry_feedback`] can
//!   only ever return the engine's evidence, and the engine only ever attaches
//!   evidence to a corroborated finding.
//! * **Green tests + failed review on the last retry gates, it does not fail.**
//!   See [`Outcome`]: there is deliberately no variant that fails a subtask.

use std::path::Path;

use sc_model::ModelBackend;
use sc_review::{Action, Finding, IntegratedDiff, ReviewConfig, ReviewOutcome, Reviewer};

use crate::board::Subtask;
use crate::event::{SwarmEvent, SwarmSink};

/// What the orchestrator should do about a review's findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to act on: review was off, skipped, found nothing, or found only
    /// uncorroborated opinions. The subtask is done.
    Clear,
    /// Re-dispatch the subtask with this feedback. Only reachable with
    /// [`Action::Retry`] and at least one corroborated finding, and only while
    /// the *existing* `max_subtask_retries` budget has room — review never gets a
    /// budget of its own, because two independent retry budgets multiply into a
    /// run that never terminates.
    Retry(String),
    /// The subtask is `Done`, but these findings are unresolved and at least
    /// `blocking` of them met the gating bar. The work is verified correct;
    /// discarding it over an unfixed finding would be the worse outcome. The run
    /// stops at a human checkpoint — or, headless, completes and reports them
    /// loudly.
    ///
    /// Note what is missing: there is no `Fail`. A green subtask is never failed
    /// by a reviewer.
    Gated { blocking: usize },
}

/// What a human decided at a review checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// Keep the findings on the record and carry on with the rest of the run.
    Continue,
    /// Stop the run here. Everything already integrated stays integrated —
    /// stopping is not reverting.
    Stop,
}

/// The human checkpoint a gating finding stops at (spec 16 — "Gate").
///
/// Deliberately not `sc_workflow::Gate`: that trait decides what to do with a
/// *phase artifact* in the staged workflow, and `sc-workflow` already depends on
/// `sc-swarm`, so reusing it directly would invert the dependency. This is the
/// same *idea* at the swarm's granularity — a human is asked, and where no human
/// is available the run completes and the findings are reported loudly rather than
/// dropped. The CLI supplies the interactive implementation; the default is
/// [`AutoContinue`].
pub trait ReviewGate {
    /// `findings` are the subtask's unresolved findings; `blocking` how many met
    /// the gating severity (always ≥ 1 when this is called).
    fn checkpoint(&self, subtask: &str, findings: &[Finding], blocking: usize) -> Checkpoint;
}

/// The headless gate: never stop. Findings are still recorded and reported —
/// "green, with reservations" — because dropping them silently would be the one
/// unacceptable outcome (spec 16).
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoContinue;

impl ReviewGate for AutoContinue {
    fn checkpoint(&self, _subtask: &str, _findings: &[Finding], _blocking: usize) -> Checkpoint {
        Checkpoint::Continue
    }
}

/// Review one subtask's integrated diff and decide what follows.
///
/// `retries_left` is what remains of the subtask's *existing* budget. With none
/// left, a corroborated finding cannot produce a retry — so it gates instead,
/// which is exactly the "last retry" case the spec calls out.
#[allow(clippy::too_many_arguments)]
pub fn review_integration(
    reviewer_backend: Option<&(dyn ModelBackend + Sync)>,
    subtask_id: &str,
    subtask: Option<&Subtask>,
    diff: &IntegratedDiff,
    workspace: &Path,
    cfg: &ReviewConfig,
    retries_left: usize,
    sink: &dyn SwarmSink,
) -> (Outcome, Vec<Finding>) {
    if !cfg.enabled {
        return (Outcome::Clear, Vec::new());
    }
    // The reviewer runs on the advisor/T1 backend, not a worker: a worker
    // reviewing a worker's output is two keyholes, not one review (spec 16). With
    // none configured there is nothing to review with, and that is a skip.
    let Some(backend) = reviewer_backend else {
        return (Outcome::Clear, Vec::new());
    };

    let reviewers = vec![Reviewer::new(backend.name(), backend)];
    let goal = subtask.map(|s| s.goal.as_str()).unwrap_or("");
    let files = subtask.map(|s| s.files.as_slice()).unwrap_or(&[]);
    let outcome = sc_review::review(&reviewers, diff, workspace, goal, files, cfg);

    // A review that never ran — the diff was below the size threshold — emits
    // NOTHING. Emitting `ReviewStarted` + `ReviewFinished { findings: 0 }` would be
    // indistinguishable from "four lenses ran and found nothing", which is the same
    // dishonesty as folding `Unknown` into `Pass` (spec 13/16). Silence on the
    // stream means the question was never asked.
    if outcome.skipped {
        return (Outcome::Clear, Vec::new());
    }

    sink.record(&SwarmEvent::ReviewStarted {
        subtask: subtask_id.to_string(),
        lenses: cfg.lenses.iter().map(|l| l.to_string()).collect(),
        reviewers: reviewers.iter().map(|r| r.id.0.clone()).collect(),
    });
    for f in &outcome.findings {
        sink.record(&SwarmEvent::review_finding(subtask_id, f));
    }
    let blocking = outcome.blocking(cfg.gate_at);
    sink.record(&SwarmEvent::ReviewFinished {
        subtask: subtask_id.to_string(),
        findings: outcome.findings.len(),
        blocking,
        reviewers_skipped: outcome
            .reviewers_skipped
            .iter()
            .map(|m| m.0.clone())
            .collect(),
    });

    (decide(&outcome, cfg, retries_left), outcome.findings)
}

/// Turn a review outcome into a decision.
///
/// The whole asymmetry in one function: an uncorroborated finding reaches neither
/// [`Outcome::Retry`] nor [`Outcome::Gated`], because `retry_feedback` yields
/// nothing without evidence and `blocking` counts only corroborated findings.
fn decide(outcome: &ReviewOutcome, cfg: &ReviewConfig, retries_left: usize) -> Outcome {
    let blocking = outcome.blocking(cfg.gate_at);
    match cfg.action {
        // Findings ride along with the report and the event stream; the run still
        // succeeds. The honest default.
        Action::Report => Outcome::Clear,
        Action::Gate => {
            if blocking > 0 {
                Outcome::Gated { blocking }
            } else {
                Outcome::Clear
            }
        }
        Action::Retry => match outcome.retry_feedback() {
            // Budget remains: re-dispatch with the evidence, exactly as
            // still-failing tests do.
            Some(feedback) if retries_left > 0 => Outcome::Retry(feedback),
            // Out of retries with a corroborated finding still standing. The
            // subtask passed its tests, so it is Done — with the finding
            // attached, and a checkpoint if it met the bar. It is never failed.
            Some(_) => {
                if blocking > 0 {
                    Outcome::Gated { blocking }
                } else {
                    Outcome::Clear
                }
            }
            None => Outcome::Clear,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NullSwarmSink;
    use sc_review::{Anchor, HunkId, Lens, ModelId, Severity};

    fn corroborated() -> Finding {
        let mut f = Finding::new(
            Lens::Duplication,
            Severity::High,
            Anchor::file("src/report/render.rs")
                .with_hunk(HunkId(0))
                .with_symbol("format_date"),
            "this looks duplicated to me",
            ModelId::new("qwen"),
        );
        f.corroborate(
            "You added `format_date` in src/report/render.rs. An equivalent already \
             exists: `format_date` already exists at src/utils/date.rs:41. Import and \
             use it instead of reimplementing it.",
        );
        f
    }

    fn opinion() -> Finding {
        Finding::new(
            Lens::AbstractionFit,
            Severity::High,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
            "I'd have written this differently",
            ModelId::new("qwen"),
        )
    }

    fn outcome(findings: Vec<Finding>) -> ReviewOutcome {
        ReviewOutcome {
            findings,
            ..Default::default()
        }
    }

    fn cfg(action: Action) -> ReviewConfig {
        ReviewConfig {
            enabled: true,
            action,
            gate_at: Severity::High,
            ..Default::default()
        }
    }

    #[test]
    fn an_uncorroborated_finding_never_retries_and_never_gates() {
        // However severe, however many reviewers raised it.
        let o = outcome(vec![opinion()]);
        assert_eq!(decide(&o, &cfg(Action::Retry), 2), Outcome::Clear);
        assert_eq!(decide(&o, &cfg(Action::Gate), 2), Outcome::Clear);
        assert_eq!(decide(&o, &cfg(Action::Report), 2), Outcome::Clear);
    }

    #[test]
    fn a_corroborated_finding_feeds_a_retry_with_the_symbol_and_its_location() {
        let o = outcome(vec![corroborated()]);
        match decide(&o, &cfg(Action::Retry), 2) {
            Outcome::Retry(feedback) => {
                assert!(feedback.contains("format_date"), "{feedback}");
                assert!(feedback.contains("src/utils/date.rs:41"), "{feedback}");
                // The reviewer's prose stays out of the worker's prompt.
                assert!(!feedback.contains("looks duplicated to me"), "{feedback}");
            }
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    #[test]
    fn green_tests_and_a_failed_review_on_the_last_retry_gates_it_does_not_fail() {
        // The case the spec is explicit about. The work is VERIFIED CORRECT;
        // throwing away green, integrated code over an unfixed finding is a worse
        // outcome than keeping it. So: Done, with findings attached, and a
        // checkpoint — never Failed. `Outcome` has no Fail variant at all.
        let o = outcome(vec![corroborated()]);
        assert_eq!(
            decide(&o, &cfg(Action::Retry), 0),
            Outcome::Gated { blocking: 1 }
        );
    }

    #[test]
    fn a_corroborated_finding_below_the_gating_severity_does_not_stop_the_run() {
        let mut low = corroborated();
        low.severity = Severity::Low;
        let o = outcome(vec![low]);
        // Out of retries and below the bar: nothing to stop for.
        assert_eq!(decide(&o, &cfg(Action::Retry), 0), Outcome::Clear);
        assert_eq!(decide(&o, &cfg(Action::Gate), 2), Outcome::Clear);
    }

    #[test]
    fn report_mode_never_stops_a_run_whatever_it_found() {
        // The honest default: a suggestion that halts a run is a tool that gets
        // switched off. Even a corroborated, high-severity finding only reports.
        let o = outcome(vec![corroborated()]);
        assert_eq!(decide(&o, &cfg(Action::Report), 0), Outcome::Clear);
    }

    #[test]
    fn review_that_is_off_costs_nothing_and_decides_nothing() {
        let (outcome, findings) = review_integration(
            None,
            "s1",
            None,
            &IntegratedDiff::default(),
            std::path::Path::new("."),
            &ReviewConfig::default(),
            2,
            &NullSwarmSink,
        );
        assert_eq!(outcome, Outcome::Clear);
        assert!(findings.is_empty());
    }

    #[test]
    fn review_with_no_reviewer_backend_is_a_skip_not_a_failure() {
        // Enabled but with no T1 backend configured: nothing to review WITH.
        let (outcome, findings) = review_integration(
            None,
            "s1",
            None,
            &IntegratedDiff::default(),
            std::path::Path::new("."),
            &cfg(Action::Gate),
            2,
            &NullSwarmSink,
        );
        assert_eq!(outcome, Outcome::Clear);
        assert!(findings.is_empty());
    }
}
