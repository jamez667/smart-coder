//! The agent event stream (spec 01 — event-stream architecture, borrowed from
//! OpenHands).
//!
//! Every meaningful thing the loop does emits a typed [`AgentEvent`] through an
//! [`EventSink`]. This is the single hub all observers consume: a live TUI, a
//! `--json` line emitter, a session log for replay. The loop itself stays
//! oblivious to *who* is watching — it just emits.
//!
//! The sink is deliberately tiny (`record(&AgentEvent)`), and the default is a
//! no-op, so adding the stream doesn't change the existing run API: callers that
//! don't care pass nothing.

use serde::Serialize;

use crate::recovery::StopReason;

/// One thing that happened during a run, in the order it happened.
///
/// Serializes to a tagged JSON object (`{"type":"ToolCall","tool":...}`) so the
/// web dashboard can render structured events off the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    /// The run began. Carries the task and the resolved prompt budget.
    RunStarted { task: String, prompt_budget: usize },
    /// The planner produced a step plan (the rendered, structured form).
    Planned { steps: Vec<String> },
    /// A model turn produced raw output (before tool extraction). `tokens` is the
    /// assembled-prompt size for this turn.
    ModelTurn { step: usize, prompt_tokens: usize },
    /// A valid tool call was decoded and is about to run (shown before it runs,
    /// spec 06). `arg` is the key argument (path/query/command), for a tight line.
    ToolCall { tool: String, arg: String },
    /// A tool produced an observation (summarized). `is_error` flags failures so a
    /// renderer can color them.
    ToolResult { summary: String, is_error: bool },
    /// The model emitted malformed output and the harness fed back a repair.
    RepairTriggered { detail: String },
    /// A verification command ran; `green` is the whole-suite result.
    Verification { green: bool, summary: String },
    /// The harness detected a loop/stall.
    Stalled { trigger: String },
    /// The advisor (senior) was consulted and returned a nudge.
    Advice { advice: String },
    /// The plan was revised mid-run (via `update_plan`).
    PlanRevised { steps: Vec<String> },
    /// The run ended. Carries the structured reason.
    Stopped { reason: StopReason },
}

/// Something that observes the event stream. The default `record` is a no-op so
/// implementors only override what they need; the loop only ever calls `record`.
pub trait EventSink {
    fn record(&self, event: &AgentEvent);
}

/// A sink that drops every event — the default when no observer is attached.
pub struct NullSink;

impl EventSink for NullSink {
    fn record(&self, _event: &AgentEvent) {}
}

/// A sink that delegates to a closure. Handy for tests (record into a `Vec`) and
/// for the TUI (send into a channel).
pub struct FnSink<F>(pub F);

impl<F: Fn(&AgentEvent)> EventSink for FnSink<F> {
    fn record(&self, event: &AgentEvent) {
        (self.0)(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn null_sink_ignores_everything() {
        let sink = NullSink;
        sink.record(&AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        // No panic, nothing recorded — the point is it's a safe no-op.
    }

    #[test]
    fn fn_sink_forwards_to_the_closure() {
        let log: RefCell<Vec<AgentEvent>> = RefCell::new(Vec::new());
        let sink = FnSink(|e: &AgentEvent| log.borrow_mut().push(e.clone()));
        sink.record(&AgentEvent::RunStarted {
            task: "do it".into(),
            prompt_budget: 5120,
        });
        sink.record(&AgentEvent::ToolCall {
            tool: "read_file".into(),
            arg: "a.rs".into(),
        });
        let recorded = log.borrow();
        assert_eq!(recorded.len(), 2);
        assert!(matches!(recorded[0], AgentEvent::RunStarted { .. }));
        assert!(matches!(recorded[1], AgentEvent::ToolCall { .. }));
    }

    #[test]
    fn events_are_cloneable_and_comparable() {
        // Renderers/loggers need to clone events off the loop's thread.
        let e = AgentEvent::Advice {
            advice: "try the modulo".into(),
        };
        assert_eq!(e.clone(), e);
    }
}
