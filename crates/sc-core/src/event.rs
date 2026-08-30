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
    /// A token delta as the model generates the current turn (only emitted when streaming is
    /// enabled via `AgentConfig::stream`). Lets a UI show the turn — including a file edit
    /// being written — appear live, word by word. `cumulative` is the full text so far this
    /// turn, so a renderer can just replace its preview rather than concatenate.
    ContentDelta { step: usize, cumulative: String },
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
    /// A root-cause diagnosis ran (a focused debugger pass over the full test output + all
    /// source files). `trigger` is the stall that prompted it; `report` is the diagnosis
    /// injected back into the loop.
    Diagnosis { trigger: String, report: String },
    /// The plan was revised mid-run (via `update_plan`).
    PlanRevised { steps: Vec<String> },
    /// A confirm-gated action is blocking the run, awaiting a human's approve/deny
    /// (spec 04 — the `Confirmer` seam). Carried on the event stream so a remote
    /// viewer (the phone) sees the pending prompt through the same `Hub` replay the
    /// rest of the stream uses — a client that reconnects mid-approval replays this
    /// with no matching `ConfirmResolved` and re-renders its approve/deny buttons.
    /// `id` correlates the later `ConfirmResolved` and the inbound approve/deny POST.
    ConfirmPending {
        id: u64,
        command: String,
        reason: String,
    },
    /// A pending confirmation was answered (by any connected client), so every
    /// viewer clears the prompt. `allowed` is whether it was approved.
    ConfirmResolved { id: u64, allowed: bool },
    /// A completed chat/planning turn in the desktop conversation, mirrored to remote
    /// clients (the phone). `role` is `"you"` / `"agent"`; `text` is the full message.
    /// Carried on the same event stream as everything else so the phone renders the live
    /// desktop chat with zero extra transport (the mirror feature).
    ChatMessage { role: String, text: String },
    /// The in-flight assistant reply as it streams, token by token — `cumulative` is the
    /// full text so far this turn (a renderer replaces its live bubble). Superseded by the
    /// terminal `ChatMessage` when the turn completes.
    ChatDelta { cumulative: String },
    /// **The harness degraded its own input or output, and knows it.**
    ///
    /// Distinct from every other event here: the rest report what the *model* did,
    /// this one reports what *we* did to it. Every harness bug found so far was
    /// first misread as the model failing -- a reply cut off at the token cap looks
    /// like a model that declined to act; a test file delivered as a directory looks
    /// like a model that ignored the test. Each cost an afternoon of transcript
    /// reading, and only the ones somebody happened to look at were ever found.
    ///
    /// A run that emits these says "3 harness faults" instead of a silent zero, so
    /// the misattribution is visible on the first turn rather than the fiftieth.
    HarnessFault {
        kind: FaultKind,
        /// What specifically happened, with the numbers -- "reply stopped at the
        /// 4096-token cap". Rendered verbatim, so it must stand alone.
        detail: String,
        /// The step it happened on, for lining up against the rest of the stream.
        step: usize,
    },
    /// The run ended. Carries the structured reason.
    Stopped { reason: StopReason },
}

/// The kinds of self-inflicted damage the harness can recognize.
///
/// Each variant exists because it was a real bug that shipped, not because it was
/// imagined: the doc comment on each names the failure it would have caught. A kind
/// with no observed firing is not a detector, so each must be forced in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultKind {
    /// The model's reply was cut off at the token cap, so whatever it was about to
    /// say -- often the tool call itself -- never arrived.
    ///
    /// Reported by the backend as `finish_reason: "length"`. Before that was
    /// plumbed through, this cost 54 turns of "no JSON tool object in your reply"
    /// on a single instance, each one blaming the model for our truncation.
    ReplyTruncated,
    /// A tool's observation was clipped by `observation_line_cap` before the model
    /// saw it, so it decided on partial evidence.
    ///
    /// This is *usually correct* -- capping a 10,000-line file is the whole point.
    /// It earns a fault only when the clipped tail plausibly carried the answer;
    /// see the emit site for the threshold. Fires on everything and the count
    /// becomes noise people learn to ignore.
    ObservationTruncated,
    /// Harness-authored text named a tool the model was never offered.
    ///
    /// Steering a model toward a tool absent from its registry is worse than
    /// silence: it spends turns trying to call something that cannot exist. One
    /// run put 99 mentions of `edit_lines` in front of a model holding six tools,
    /// none of them that one.
    ToolNotOffered,
    /// An error message promised the model context and then showed none.
    ///
    /// The ambiguous-anchor message said "the anchor matched multiple places" and
    /// listed zero of them, eight times in a row. A model cannot disambiguate from
    /// an empty list, so it guessed, and the guesses looked like incompetence.
    EmptyGuidance,
    /// A read failed on a path the harness itself put there.
    ///
    /// Never the model's fault by construction. Test files were once delivered as
    /// directories across 185 files of a subset, and every run read them as
    /// "failed read" and moved on.
    UnreadablePath,
    /// The verify command cannot run -- its binary is not on PATH.
    ///
    /// A missing `pytest` scores as the model failing to make tests pass, which is
    /// indistinguishable from a wrong patch if you only read the score.
    VerifyUnavailable,
}

impl FaultKind {
    /// A short, stable label for rendering and for grouping counts.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReplyTruncated => "reply truncated",
            Self::ObservationTruncated => "observation truncated",
            Self::ToolNotOffered => "tool not offered",
            Self::EmptyGuidance => "empty guidance",
            Self::UnreadablePath => "unreadable path",
            Self::VerifyUnavailable => "verify unavailable",
        }
    }
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

/// A standing **prompt-transcript** sink: writes a human-readable record of *exactly what
/// the model saw and said* each turn — the full assembled prompt (`PromptAssembled`), the
/// raw reply (`ModelTurn`), the observation it got back (`ToolResult`/`Verification`), and
/// stalls/advice/stops. This is the "dump the exact prompt" capability the whole multi-file
/// debugging effort relied on, promoted from throwaway probe examples to a reusable sink:
/// attach it (typically via [`TeeSink`]) to any run with `verbose` on and read the `.md` it
/// produces. Requires `AgentConfig.verbose = true` for the prompt bodies (that's what gates
/// `PromptAssembled`); without it you still get the replies/observations, just not the
/// assembled prompt.
///
/// Like [`JsonLinesSink`], the writer is behind a `Mutex` so the sink is `Sync`, and write
/// failures are swallowed — observation must never break a run.
pub struct TranscriptSink<W: Write> {
    writer: Mutex<W>,
}

impl<W: Write> TranscriptSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    pub fn into_inner(self) -> W {
        self.writer.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    /// Render one event as a readable transcript block (empty string for events we don't
    /// surface in a prompt transcript — e.g. `ToolCall`, which is implied by the reply).
    fn render(event: &AgentEvent) -> String {
        match event {
            AgentEvent::RunStarted {
                task,
                prompt_budget,
            } => {
                let head: String = task.chars().take(120).collect();
                format!(
                    "\n\n========================= NEW RUN =========================\n\
                     task: {head}\nprompt_budget: {prompt_budget}\n"
                )
            }
            AgentEvent::PromptAssembled {
                step,
                tokens,
                messages,
            } => {
                let mut s = format!(
                    "\n---------------- TURN {step} — ASSEMBLED PROMPT ({tokens} tokens) ----------------\n"
                );
                for m in messages {
                    s.push_str(&format!("[[{}]]\n{}\n", m.role, m.content));
                }
                s
            }
            AgentEvent::ModelTurn { step, raw, .. } => {
                format!("\n>>> TURN {step} — MODEL REPLY:\n{raw}\n")
            }
            AgentEvent::ToolResult { full, is_error, .. } => {
                let tag = if *is_error {
                    "OBSERVATION (error)"
                } else {
                    "OBSERVATION"
                };
                format!("<<< {tag}:\n{full}\n")
            }
            AgentEvent::Verification { green, full, .. } => {
                format!("<<< VERIFICATION (green={green}):\n{full}\n")
            }
            AgentEvent::RepairTriggered { detail } => format!("!! REPAIR: {detail}\n"),
            AgentEvent::Stalled { trigger } => format!("** STALLED: {trigger}\n"),
            AgentEvent::Advice { trigger, advice } => {
                format!("** ADVICE ({trigger}):\n{advice}\n")
            }
            AgentEvent::Diagnosis { trigger, report } => {
                format!("** DIAGNOSIS ({trigger}):\n{report}\n")
            }
            // Rendered into the transcript deliberately: a fault is context the model
            // needs. "your last reply was cut off" explains the repair prompt it is
            // about to get, and reads very differently from silence.
            AgentEvent::HarnessFault { kind, detail, .. } => {
                format!("** HARNESS FAULT ({}): {detail}\n", kind.label())
            }
            AgentEvent::Stopped { reason } => format!("** STOPPED: {reason:?}\n"),
            // Not surfaced in a prompt transcript (covered by the reply / prompt blocks; a
            // ContentDelta is just the live-view increment of the ModelTurn reply;
            // Confirm* events are live-view approval prompts, not transcript content).
            AgentEvent::ToolCall { .. }
            | AgentEvent::Planned { .. }
            | AgentEvent::PlanRevised { .. }
            | AgentEvent::ContentDelta { .. }
            | AgentEvent::ConfirmPending { .. }
            | AgentEvent::ConfirmResolved { .. }
            | AgentEvent::ChatMessage { .. }
            | AgentEvent::ChatDelta { .. } => String::new(),
        }
    }
}

impl<W: Write> EventSink for TranscriptSink<W> {
    fn record(&self, event: &AgentEvent) {
        let block = Self::render(event);
        if block.is_empty() {
            return;
        }
        if let Ok(mut w) = self.writer.lock() {
            let _ = write!(w, "{block}");
            let _ = w.flush();
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
    fn transcript_sink_renders_a_readable_prompt_dump() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let sink = TranscriptSink::new(&mut buf);
            sink.record(&AgentEvent::RunStarted {
                task: "build app".into(),
                prompt_budget: 17408,
            });
            sink.record(&AgentEvent::PromptAssembled {
                step: 1,
                tokens: 790,
                messages: vec![
                    PromptMessage {
                        role: "system".into(),
                        content: "you are an agent".into(),
                    },
                    PromptMessage {
                        role: "user".into(),
                        content: "write store.py".into(),
                    },
                ],
            });
            sink.record(&AgentEvent::ModelTurn {
                step: 1,
                prompt_tokens: 790,
                raw: r#"{"tool":"write_file","path":"store.py","content":"x=1"}"#.into(),
            });
            sink.record(&AgentEvent::ToolResult {
                summary: "write_file store.py ok".into(),
                full: "write_file store.py ok (3 bytes)".into(),
                is_error: false,
            });
            // A ToolCall is intentionally NOT surfaced (implied by the reply).
            sink.record(&AgentEvent::ToolCall {
                tool: "write_file".into(),
                arg: "store.py".into(),
            });
            sink.record(&AgentEvent::Stopped {
                reason: StopReason::Finished,
            });
        }
        let text = String::from_utf8(buf).unwrap();
        // The assembled prompt (both roles) is present verbatim — the whole point.
        assert!(text.contains("ASSEMBLED PROMPT (790 tokens)"));
        assert!(text.contains("[[system]]\nyou are an agent"));
        assert!(text.contains("[[user]]\nwrite store.py"));
        // The raw reply and the observation are present.
        assert!(text.contains("MODEL REPLY:") && text.contains(r#""tool":"write_file""#));
        assert!(text.contains("OBSERVATION:") && text.contains("ok (3 bytes)"));
        assert!(text.contains("STOPPED: Finished"));
        // ToolCall produced no block.
        assert!(!text.contains("write_file\", arg"));
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
