//! Swarm events (spec 08 — determinism & inspection): the orchestrator's own
//! event stream, parallel to the per-worker `dc_core` event streams.
//!
//! These let a UI render swarm-level state — decomposition, which workers are
//! running which subtasks, and how each integration resolved — on top of the
//! per-worker activity.

use serde::{Deserialize, Serialize};

/// One orchestrator-level event.
///
/// `Serialize`/`Deserialize` so the stream round-trips: a `--json` swarm run
/// emits one NDJSON line per event (mirroring `dc_core::AgentEvent`), and the
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
    /// The whole swarm run ended.
    SwarmDone {
        done: usize,
        failed: usize,
        all_done: bool,
    },
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
        // output is re-parseable (parity with `dc_core::AgentEvent`).
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
}
