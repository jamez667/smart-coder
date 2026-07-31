//! Swarm events (spec 08 — determinism & inspection): the orchestrator's own
//! event stream, parallel to the per-worker `sc_core` event streams.
//!
//! These let a UI render swarm-level state — decomposition, which workers are
//! running which subtasks, and how each integration resolved — on top of the
//! per-worker activity.

use serde::{Deserialize, Serialize};

/// One orchestrator-level event.
///
/// `Serialize`/`Deserialize` so the stream round-trips: a `--json` swarm run
/// emits one NDJSON line per event (mirroring `sc_core::AgentEvent`), and the
/// same line parses back for replay/inspection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SwarmEvent {
    /// The task was decomposed into these subtask goals.
    Decomposed { subtasks: Vec<String> },
    /// The exact prompt sent to the orchestrator to decompose the task, and its raw
    /// reply — so a UI can show *what was asked and answered* (and whether it fell back
    /// to a trivial split). Emitted once, before [`Decomposed`].
    OrchestratorPrompt {
        prompt: String,
        reply: String,
        fell_back: bool,
    },
    /// A worker began a subtask. `prompt` is the full single-shot prompt it was handed
    /// (the goal + the current contents of its scoped files) — what the coder "saw".
    WorkerStarted {
        subtask: String,
        goal: String,
        prompt: String,
    },
    /// A worker finished its run (before integration). `summary` is the one-line
    /// report ("proposed a fix (N words)"); `proposal` is the worker's full proposed
    /// file content, so a UI can show *what* it produced, not just that it did.
    WorkerFinished {
        subtask: String,
        summary: String,
        proposal: String,
    },
    /// A subtask is being re-dispatched after an incomplete/rejected integration
    /// (spec 08 — "Subtask retry on partial or rejected integration"). Emitted
    /// before each re-dispatch. `attempt` is the retry number (1-based, so the
    /// first retry is `1`), `max` the configured `max_subtask_retries`, and
    /// `failing_tests` the still-red scoped tests that motivated the retry.
    SubtaskRetry {
        subtask: String,
        attempt: usize,
        max: usize,
        failing_tests: Vec<String>,
    },
    /// The orchestrator escalated to the advisor ("junior asks senior", spec 02/08)
    /// before a subtask's **final** retry, and got a one-line nudge folded into the
    /// next worker prompt. Advice, not the fix — the worker still does the work.
    AdvisorConsulted { subtask: String, advice: String },
    /// A worker's proposal was integrated (accepted) or rejected. On accept,
    /// `files` are the changed paths; on reject, `files[0]` is the reason.
    Integrated {
        subtask: String,
        accepted: bool,
        files: Vec<String>,
    },
    /// Post-integration review began over a subtask's integrated diff (spec 16 —
    /// a second gate, after verification, asking *should this code stay?* rather
    /// than *does it work?*). `lenses` are the questions being asked and
    /// `reviewers` who is being asked them; cost is `lenses × reviewers`, which is
    /// why both are surfaced before the calls rather than after.
    ReviewStarted {
        subtask: String,
        lenses: Vec<String>,
        reviewers: Vec<String>,
    },
    /// One review finding. Emitted per finding so a renderer can show them as they
    /// land rather than waiting for the whole review.
    ///
    /// `corroborated` is the load-bearing field: a deterministic check agreed, and
    /// only a corroborated finding may gate the run or feed a retry. `evidence` is
    /// what that check found — the text injected into a retry prompt — while
    /// `summary` is the reviewer's prose, for a human reading the report. The two
    /// are never interchanged. `raised_by` is who saw it; `considered_by` is who
    /// reviewed this diff at all, which is what makes a lone finding interpretable
    /// (contested vs merely unreviewed).
    ReviewFinding {
        subtask: String,
        lens: String,
        severity: String,
        /// `file`, `hunk`, `symbol`, and a `line` that is a render hint only —
        /// findings are never identified by line number (spec 16 — anchoring).
        anchor: ReviewAnchor,
        corroborated: bool,
        evidence: Option<String>,
        raised_by: Vec<String>,
        considered_by: Vec<String>,
        summary: String,
    },
    /// Review finished for a subtask.
    ///
    /// `blocking` is the count of findings that met the bar to stop the run —
    /// corroborated AND at or above the configured gating severity. It is carried
    /// rather than left for a renderer to recompute, so every surface agrees on
    /// whether a review stopped anything. Zero is the normal case.
    ///
    /// `reviewers_skipped` is carried explicitly rather than inferred from a
    /// shorter `considered_by`: a renderer must be able to say "3 of 4 reviewers
    /// ran" instead of quietly reporting a narrower review as a complete one.
    ReviewFinished {
        subtask: String,
        findings: usize,
        blocking: usize,
        reviewers_skipped: Vec<String>,
    },
    /// The whole swarm run ended.
    SwarmDone {
        done: usize,
        failed: usize,
        all_done: bool,
    },
}

/// Where a review finding points, on the wire (spec 16 — anchoring).
///
/// A flattened [`sc_review::Anchor`]: the event stream is a public surface, so it
/// carries plain strings rather than re-exporting the engine's types through it.
/// `line` is a **render hint** — resolved from the hunk, never trusted from the
/// model, and never used to identify or match a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnchor {
    pub file: String,
    pub hunk: Option<usize>,
    pub symbol: Option<String>,
    pub line: Option<usize>,
}

impl From<&sc_review::Anchor> for ReviewAnchor {
    fn from(a: &sc_review::Anchor) -> Self {
        Self {
            file: a.file.clone(),
            hunk: a.hunk.map(|h| h.0),
            symbol: a.symbol.clone(),
            line: a.line,
        }
    }
}

impl SwarmEvent {
    /// A [`SwarmEvent::ReviewFinding`] for one finding of `subtask`.
    pub fn review_finding(subtask: &str, f: &sc_review::Finding) -> Self {
        SwarmEvent::ReviewFinding {
            subtask: subtask.to_string(),
            lens: f.lens.to_string(),
            severity: f.severity.to_string(),
            anchor: ReviewAnchor::from(&f.anchor),
            corroborated: f.corroborated,
            evidence: f.evidence.clone(),
            raised_by: f.raised_by.iter().map(|m| m.0.clone()).collect(),
            considered_by: f.considered_by.iter().map(|m| m.0.clone()).collect(),
            summary: f.summary.clone(),
        }
    }
}

/// Observer of the swarm event stream.
pub trait SwarmSink {
    fn record(&self, event: &SwarmEvent);
}

/// A no-op sink (the default when nothing is watching).
pub struct NullSwarmSink;
impl SwarmSink for NullSwarmSink {
    fn record(&self, _event: &SwarmEvent) {}
}

/// A closure-backed sink (tests record into a Vec; a UI forwards to a channel).
pub struct FnSwarmSink<F>(pub F);
impl<F: Fn(&SwarmEvent)> SwarmSink for FnSwarmSink<F> {
    fn record(&self, event: &SwarmEvent) {
        (self.0)(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn null_sink_ignores() {
        NullSwarmSink.record(&SwarmEvent::SwarmDone {
            done: 1,
            failed: 0,
            all_done: true,
        });
    }

    #[test]
    fn fn_sink_records_and_serializes() {
        let log: RefCell<Vec<SwarmEvent>> = RefCell::new(Vec::new());
        let sink = FnSwarmSink(|e: &SwarmEvent| log.borrow_mut().push(e.clone()));
        sink.record(&SwarmEvent::WorkerStarted {
            subtask: "a".into(),
            goal: "do a".into(),
            prompt: "Task: do a".into(),
        });
        assert_eq!(log.borrow().len(), 1);
        let json = serde_json::to_string(&log.borrow()[0]).unwrap();
        assert!(json.contains("\"type\":\"WorkerStarted\""), "{json}");
    }

    #[test]
    fn event_round_trips_through_json() {
        // Every variant must survive Serialize→Deserialize so `--json` swarm
        // output is re-parseable (parity with `sc_core::AgentEvent`).
        let events = vec![
            SwarmEvent::Decomposed {
                subtasks: vec!["a".into(), "b".into()],
            },
            SwarmEvent::OrchestratorPrompt {
                prompt: "Break the task…".into(),
                reply: "[{\"id\":\"t1\"}]".into(),
                fell_back: false,
            },
            SwarmEvent::WorkerStarted {
                subtask: "s1".into(),
                goal: "do the thing".into(),
                prompt: "Task: do the thing".into(),
            },
            SwarmEvent::WorkerFinished {
                subtask: "s1".into(),
                summary: "edited 1 file".into(),
                proposal: "the proposed file body".into(),
            },
            SwarmEvent::SubtaskRetry {
                subtask: "s1".into(),
                attempt: 1,
                max: 2,
                failing_tests: vec!["test_upper_bound".into(), "test_clamp".into()],
            },
            SwarmEvent::AdvisorConsulted {
                subtask: "s1".into(),
                advice: "clamp the upper bound too: min(hi, max(lo, x))".into(),
            },
            SwarmEvent::Integrated {
                subtask: "s1".into(),
                accepted: true,
                files: vec!["src/lib.rs".into()],
            },
            SwarmEvent::Integrated {
                subtask: "s2".into(),
                accepted: false,
                files: vec!["suite went red".into()],
            },
            SwarmEvent::ReviewStarted {
                subtask: "s1".into(),
                lenses: vec!["duplication".into(), "error-handling".into()],
                reviewers: vec!["qwen".into()],
            },
            SwarmEvent::ReviewFinding {
                subtask: "s1".into(),
                lens: "duplication".into(),
                severity: "high".into(),
                anchor: ReviewAnchor {
                    file: "src/report/render.rs".into(),
                    hunk: Some(0),
                    symbol: Some("format_date".into()),
                    line: Some(12),
                },
                corroborated: true,
                evidence: Some("`format_date` already exists at src/utils/date.rs:41".into()),
                raised_by: vec!["qwen".into()],
                considered_by: vec!["qwen".into(), "gemini".into()],
                summary: "reimplements the date helper".into(),
            },
            // An uncorroborated finding: no evidence, and no anchor beyond the file.
            SwarmEvent::ReviewFinding {
                subtask: "s1".into(),
                lens: "abstraction-fit".into(),
                severity: "low".into(),
                anchor: ReviewAnchor {
                    file: "src/a.rs".into(),
                    hunk: None,
                    symbol: None,
                    line: None,
                },
                corroborated: false,
                evidence: None,
                raised_by: vec!["qwen".into()],
                considered_by: vec!["qwen".into()],
                summary: "doesn't match the surrounding style".into(),
            },
            SwarmEvent::ReviewFinished {
                subtask: "s1".into(),
                findings: 2,
                blocking: 1,
                reviewers_skipped: vec!["offline".into()],
            },
            SwarmEvent::SwarmDone {
                done: 2,
                failed: 1,
                all_done: false,
            },
        ];
        for ev in &events {
            let line = serde_json::to_string(ev).unwrap();
            let back: SwarmEvent = serde_json::from_str(&line).unwrap();
            assert_eq!(&back, ev, "round-trip mismatch for {line}");
        }
    }

    #[test]
    fn a_review_finding_event_carries_evidence_and_summary_separately() {
        // The two must never be interchanged: `evidence` is what a worker is
        // handed on a retry, `summary` is what a human reads in the report.
        let mut f = sc_review::Finding::new(
            sc_review::Lens::Duplication,
            sc_review::Severity::High,
            sc_review::Anchor::file("src/report/render.rs")
                .with_hunk(sc_review::HunkId(3))
                .with_symbol("format_date")
                .with_line(12),
            "this smells like a duplicate",
            sc_review::ModelId::new("qwen"),
        );
        f.corroborate("`format_date` already exists at src/utils/date.rs:41");
        f.considered_by = vec![sc_review::ModelId::new("qwen")];

        let SwarmEvent::ReviewFinding {
            anchor,
            corroborated,
            evidence,
            summary,
            severity,
            lens,
            raised_by,
            ..
        } = SwarmEvent::review_finding("s1", &f)
        else {
            panic!("expected a ReviewFinding");
        };
        assert_eq!(lens, "duplication");
        assert_eq!(severity, "high");
        assert_eq!(anchor.hunk, Some(3));
        assert_eq!(anchor.symbol.as_deref(), Some("format_date"));
        assert_eq!(anchor.line, Some(12), "a render hint, carried as such");
        assert!(corroborated);
        assert!(evidence.unwrap().contains("src/utils/date.rs:41"));
        assert_eq!(summary, "this smells like a duplicate");
        assert_eq!(raised_by, vec!["qwen".to_string()]);
    }
}
