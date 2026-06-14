//! Swarm events (spec 08 — determinism & inspection): the orchestrator's own
//! event stream, parallel to the per-worker `dc_core` event streams.
//!
//! These let a UI render swarm-level state — decomposition, which workers are
//! running which subtasks, and how each integration resolved — on top of the
//! per-worker activity.

use serde::Serialize;

/// One orchestrator-level event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum SwarmEvent {
    /// The task was decomposed into these subtask goals.
    Decomposed { subtasks: Vec<String> },
    /// A worker began a subtask.
    WorkerStarted { subtask: String, goal: String },
    /// A worker finished its run (before integration).
    WorkerFinished { subtask: String, summary: String },
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
        });
        assert_eq!(log.borrow().len(), 1);
        let json = serde_json::to_string(&log.borrow()[0]).unwrap();
        assert!(json.contains("\"type\":\"WorkerStarted\""), "{json}");
    }
}
