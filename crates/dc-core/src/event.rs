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

use std::io::Write;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::recovery::StopReason;

/// One message of an assembled prompt — a role + its content — carried by
/// [`AgentEvent::PromptAssembled`] so a verbose renderer can show the full prompt
/// the model saw (spec 06). Role is a plain string (`"system"`/`"user"`/
/// `"assistant"`) so the event stays serializable without leaking the model crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: String,
}

/// One thing that happened during a run, in the order it happened.
///
/// Serializes to a tagged JSON object (`{"type":"ToolCall","tool":...}`) so the
/// web dashboard can render structured events off the wire — and a `--json`
/// emitter / session log can write them as JSON lines and `replay` read them back
/// (spec 06).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    /// The run began. Carries the task and the resolved prompt budget.
    RunStarted { task: String, prompt_budget: usize },
    /// The planner produced a step plan (the rendered, structured form).
    Planned { steps: Vec<String> },
    /// The fully-assembled, budgeted prompt for a turn — *exactly* what the model
    /// is about to see (spec 06 `--verbose`, spec 05). Only emitted when verbose is
    /// on (the payload is large), so normal runs/logs stay lean. One entry per
    /// message in send order.
    PromptAssembled {
        step: usize,
        tokens: usize,
        messages: Vec<PromptMessage>,
    },
    /// A model turn. `prompt_tokens` is the assembled-prompt size; `raw` is the
    /// model's *full* raw output for that turn (before tool extraction) so a UI
    /// can show exactly what the model said, reasoning and all.
    ModelTurn {
        step: usize,
        prompt_tokens: usize,
        raw: String,
    },
    /// A valid tool call was decoded and is about to run (shown before it runs,
    /// spec 06). `arg` is the key argument (path/query/command), for a tight line.
    ToolCall { tool: String, arg: String },
    /// A tool produced an observation. `summary` is the first line (for a tight
    /// view); `full` is the complete result (for an expanded view). `is_error`
    /// flags failures so a renderer can color them.
    ToolResult {
        summary: String,
        full: String,
        is_error: bool,
    },
    /// The model emitted malformed output and the harness fed back a repair.
    RepairTriggered { detail: String },
    /// A verification command ran; `green` is the whole-suite result. `full` is
    /// the complete structured report text.
    Verification {
        green: bool,
        summary: String,
        full: String,
    },
    /// The harness detected a loop/stall.
    Stalled { trigger: String },
    /// The advisor (senior) was consulted. `trigger` is why it was asked; `advice`
    /// is the full hint it returned — the complete junior↔senior exchange.
    Advice { trigger: String, advice: String },
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

/// A sink that writes each event as one JSON line (NDJSON) to any [`Write`] — the
/// `--json` emitter (to stdout) and the session log (to a file) are the same sink
/// over different writers (spec 06). `replay` reads the lines back via
/// [`AgentEvent`]'s `Deserialize`.
///
/// The writer is behind a `Mutex` so the sink is `Sync`: the TUI drives the loop
/// on a worker thread and shares the sink as `&dyn EventSink`. A serialize/write
/// failure is swallowed — observation must never break the run.
pub struct JsonLinesSink<W: Write> {
    writer: Mutex<W>,
}

impl<W: Write> JsonLinesSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    /// Recover the inner writer (e.g. to flush/close a log file after the run).
    pub fn into_inner(self) -> W {
        self.writer.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write> EventSink for JsonLinesSink<W> {
    fn record(&self, event: &AgentEvent) {
        if let Ok(line) = serde_json::to_string(event) {
            if let Ok(mut w) = self.writer.lock() {
                let _ = writeln!(w, "{line}");
                let _ = w.flush();
            }
        }
    }
}

/// A sink that fans every event out to several others — so a live renderer and a
/// session log can both observe one run without changing the loop's single-sink
/// signature (spec 01/06). Each delegate is called in order.
pub struct TeeSink<'a> {
    sinks: Vec<&'a dyn EventSink>,
}

impl<'a> TeeSink<'a> {
    pub fn new(sinks: Vec<&'a dyn EventSink>) -> Self {
        Self { sinks }
    }
}

impl EventSink for TeeSink<'_> {
    fn record(&self, event: &AgentEvent) {
        for sink in &self.sinks {
            sink.record(event);
        }
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
    fn json_lines_sink_writes_one_tagged_object_per_event() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let sink = JsonLinesSink::new(&mut buf);
            sink.record(&AgentEvent::RunStarted {
                task: "do it".into(),
                prompt_budget: 5120,
            });
            sink.record(&AgentEvent::ToolCall {
                tool: "read_file".into(),
                arg: "a.rs".into(),
            });
            sink.record(&AgentEvent::Stopped {
                reason: StopReason::Finished,
            });
        }
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one line per event: {text:?}");
        // Each line is a standalone JSON object carrying the type tag.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("type").is_some(), "missing tag in {line:?}");
        }
        assert_eq!(
            lines[0],
            r#"{"type":"RunStarted","task":"do it","prompt_budget":5120}"#
        );
    }

    #[test]
    fn agent_event_round_trips_through_json() {
        // replay relies on this: every event Serialize→Deserialize is lossless.
        let events = vec![
            AgentEvent::RunStarted {
                task: "t".into(),
                prompt_budget: 42,
            },
            AgentEvent::Planned {
                steps: vec!["a".into(), "b".into()],
            },
            AgentEvent::ModelTurn {
                step: 1,
                prompt_tokens: 10,
                raw: "raw".into(),
            },
            AgentEvent::Verification {
                green: true,
                summary: "ok".into(),
                full: "all ok".into(),
            },
            AgentEvent::Advice {
                trigger: "stall".into(),
                advice: "try modulo".into(),
            },
            AgentEvent::Stopped {
                reason: StopReason::Stalled("looping".into()),
            },
        ];
        for e in events {
            let line = serde_json::to_string(&e).unwrap();
            let back: AgentEvent = serde_json::from_str(&line).unwrap();
            assert_eq!(back, e, "round-trip mismatch for {line}");
        }
    }

    #[test]
    fn tee_sink_fans_out_to_every_delegate() {
        let a: RefCell<Vec<AgentEvent>> = RefCell::new(Vec::new());
        let b: RefCell<Vec<AgentEvent>> = RefCell::new(Vec::new());
        let sa = FnSink(|e: &AgentEvent| a.borrow_mut().push(e.clone()));
        let sb = FnSink(|e: &AgentEvent| b.borrow_mut().push(e.clone()));
        let tee = TeeSink::new(vec![&sa, &sb]);
        tee.record(&AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        assert_eq!(a.borrow().len(), 1);
        assert_eq!(b.borrow().len(), 1);
        assert_eq!(a.borrow()[0], b.borrow()[0]);
    }

    #[test]
    fn events_are_cloneable_and_comparable() {
        // Renderers/loggers need to clone events off the loop's thread.
        let e = AgentEvent::Advice {
            trigger: "looping".into(),
            advice: "try the modulo".into(),
        };
        assert_eq!(e.clone(), e);
    }
}
