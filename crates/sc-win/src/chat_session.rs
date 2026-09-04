//! [`ChatSession`] — runs one chat turn (a `backend.generate` call) on a worker thread and
//! streams the result back to the UI, mirroring [`crate::session::Session`]. The model call
//! is blocking and slow; it must never run on the iced thread, so the app spawns a
//! `ChatSession` per user turn and drains its channel each frame.
//!
//! The conversation state itself lives in the app (a [`crate::chat::Conversation`]); this
//! just carries one request out and one reply back. Nothing here is an iced type, so the
//! spawn/stream flow is host-testable.

use std::sync::mpsc::Receiver;
use std::thread;

use sc_model::{GenerateRequest, ModelBackend};

use crate::chat::{ChatIntent, Conversation};
use crate::config::UiConfig;

/// What a worker reports when there is no model to talk to (Craft mode, spec 21).
///
/// Shared so every seam says the same thing: the mode is a deliberate setting, not a failure, and
/// the message points at where to change it rather than reading as an error.
pub const NO_MODEL: &str =
    "Craft mode is on — no model is contacted. Turn it off in Settings ▸ General to use the agent.";

/// The result of one chat turn streamed back to the UI.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// A token delta as the model generates it — appended to the in-flight bubble live
    /// (the "watch it type" effect).
    Token(String),
    /// The turn finished: the full concatenated reply, plus the classified [`ChatIntent`] (so
    /// the app can, e.g., wrap a bare-prose feature plan into a PLAN file). The app parses the
    /// text for plan-file blocks / `<think>` stripping (the streamed tokens were the raw live
    /// view). `intent` is `None` for a non-classified turn (the plain `spawn` path).
    ///
    /// `truncated` is the backend's `finish_reason == "length"`: the reply hit the token cap
    /// rather than finishing. It used to be dropped here, which is how a reasoning model that
    /// spent its ENTIRE budget thinking arrived as a silent empty bubble — the signal saying
    /// so was in the response all along and nothing looked at it.
    Reply {
        text: String,
        intent: Option<ChatIntent>,
        truncated: bool,
    },
    /// The turn failed (backend unreachable, etc.) — a human-readable reason.
    Failed(String),
}

/// A single in-flight chat turn. Holds the receiving end the UI drains; the worker owns the
/// sender and the backend. Dropping it lets the worker finish in the background.
pub struct ChatSession {
    events: Receiver<ChatEvent>,
    _handle: thread::JoinHandle<()>,
    /// Cooperative cancel flag shared with the worker's backend: set true to abort the
    /// in-flight streaming turn at the next SSE line.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ChatSession {
    /// Spawn one chat turn: build the coder backend from `cfg`, run `generate(req)` on a
    /// worker thread, and stream back a [`ChatEvent`]. The caller passes the fully-built
    /// [`GenerateRequest`] (from `Conversation::request`) so this stays free of chat state.
    pub fn spawn(cfg: UiConfig, req: GenerateRequest) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let handle = thread::spawn(move || {
            // `None` in Craft mode (spec 21) — no model is contacted, so the turn reports why
            // instead of dialling out. The UI that starts a chat is hidden there; this is the
            // seam that holds even if some path reaches here anyway.
            let Some(backend) = cfg.backend_cancellable(worker_cancel) else {
                let _ = tx.send(ChatEvent::Failed(NO_MODEL.to_string()));
                return;
            };
            // Stream tokens live (the "watch it type" effect); on completion send the full
            // Reply so the app can parse plan-file blocks / strip <think> from the whole text.
            let tok_tx = tx.clone();
            let mut on_token = |delta: &str| {
                let _ = tok_tx.send(ChatEvent::Token(delta.to_string()));
            };
            let result = backend.generate_streaming(&req, &mut on_token);
            match result {
                Ok(resp) => {
                    let truncated = resp.was_truncated();
                    let _ = tx.send(ChatEvent::Reply {
                        text: resp.content,
                        intent: None,
                        truncated,
                    });
                }
                Err(e) => {
                    let _ = tx.send(ChatEvent::Failed(format!("chat failed: {e}")));
                }
            }
        });
        Self {
            events: rx,
            _handle: handle,
            cancel,
        }
    }

    /// Request cancellation of the in-flight turn: the streaming backend stops at its next
    /// SSE line and the worker sends its (partial) reply. Idempotent.
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Spawn a full planning turn: first CLASSIFY the user's intent (a fast, grammar-constrained
    /// call whose reply is one intent token), then GENERATE with an instruction tailored to that
    /// intent (and, for file-producing intents, a grammar that forces the right `file:` block).
    /// Both calls run on the worker thread; only the generate call streams tokens to the UI. This
    /// replaces string-sniffing the reply for intent — the model classifies, the app doesn't
    /// guess. `think` controls the generate reasoning budget.
    /// `workspace` enables the INVESTIGATE path: a question about the code runs the
    /// read-only agent loop so the model can read its way to an answer. `None` (no project
    /// open) keeps the old prose-only behaviour, which is all that is possible without a
    /// tree to read.
    pub fn spawn_planning(
        cfg: UiConfig,
        convo: Conversation,
        think: bool,
        workspace: Option<std::path::PathBuf>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let handle = thread::spawn(move || {
            let Some(backend) = cfg.backend_cancellable(worker_cancel) else {
                let _ = tx.send(ChatEvent::Failed(NO_MODEL.to_string()));
                return;
            };
            // 1) Classify. On any failure, fall back to Question (prose-only — the safe default).
            let intent = match backend.generate(&convo.classify_request()) {
                Ok(resp) => ChatIntent::parse(&resp.content),
                Err(_) => ChatIntent::Question,
            };
            // 1b) A QUESTION about the code is answered by READING the code.
            //
            // Everything below this branch is a single completion with no tools: the model
            // sees the README, the TODO and whichever file happens to be open, and nothing
            // else. Asked why a star trail was thin before it was thick, it said "I can't
            // see the jump screen rendering code", guessed, and asked to be pointed at the
            // file — the only move it had. With a workspace it now searches and reads.
            if intent == ChatIntent::Question {
                if let Some(ws) = workspace {
                    investigate_into_chat(&cfg, convo.last_user_message(), &ws, &tx);
                    return;
                }
            }
            // 2) Generate the actual reply, tailored to the classified intent, streamed live.
            let req = convo.request(think, intent);
            let tok_tx = tx.clone();
            let mut on_token = |delta: &str| {
                let _ = tok_tx.send(ChatEvent::Token(delta.to_string()));
            };
            match backend.generate_streaming(&req, &mut on_token) {
                Ok(resp) => {
                    let truncated = resp.was_truncated();
                    let _ = tx.send(ChatEvent::Reply {
                        text: resp.content,
                        intent: Some(intent),
                        truncated,
                    });
                }
                Err(e) => {
                    let _ = tx.send(ChatEvent::Failed(format!("chat failed: {e}")));
                }
            }
        });
        Self {
            events: rx,
            _handle: handle,
            cancel,
        }
    }

    /// Non-blocking drain of any events that have arrived since the last call.
    pub fn drain(&self) -> Vec<ChatEvent> {
        self.events.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::Message;

    /// A spawned chat turn against an unreachable backend still yields a terminal event
    /// (Failed) rather than hanging — the UI always learns the turn ended. Mirrors the
    /// `Session` unreachable-backend test.
    #[test]
    fn unreachable_backend_yields_a_failed_event() {
        let cfg = UiConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "none".to_string(),
            ..UiConfig::default()
        };
        let req = GenerateRequest::new(vec![Message::user("hi")]);
        let session = ChatSession::spawn(cfg, req);

        // Block for the terminal event (Err means the sender dropped).
        let ev = session.events.recv().ok();
        assert!(
            matches!(
                ev,
                Some(ChatEvent::Failed(_)) | Some(ChatEvent::Reply { .. })
            ),
            "expected a terminal ChatEvent, got {ev:?}"
        );
    }
}

#[cfg(test)]
mod truncation_is_reported {
    use super::*;

    /// **A truncated reply must be distinguishable from a finished one.**
    ///
    /// Measured against Tiel: it spent all 700 completion tokens in
    /// `reasoning_content` and returned `content: ""` with
    /// `finish_reason: "length"`. The event dropped that flag, so the UI showed an
    /// empty bubble and the model looked broken. The flag now rides along.
    #[test]
    fn the_reply_event_carries_the_truncation_flag() {
        let cut = ChatEvent::Reply {
            text: String::new(),
            intent: None,
            truncated: true,
        };
        match cut {
            ChatEvent::Reply {
                truncated, text, ..
            } => {
                assert!(truncated, "a capped reply must say so");
                assert!(text.is_empty(), "the measured case had no content at all");
            }
            _ => panic!("wrong variant"),
        }

        // A healthy reply is not flagged, or the warning becomes noise people ignore.
        let ok = ChatEvent::Reply {
            text: "done".into(),
            intent: None,
            truncated: false,
        };
        assert!(matches!(
            ok,
            ChatEvent::Reply {
                truncated: false,
                ..
            }
        ));
    }
}

/// What the panel says before any slow work starts.
///
/// ONE LINE on purpose: a backslash-continued literal had its continuation reflowed by
/// rustfmt into literal spaces, and the user saw "and come          back with the".
const INVESTIGATION_PLAN: &str =
    "Looking through the code to answer that — I'll read the relevant files and come back with the file, the line, and what to change.\n\n";

/// Run a read-only agent loop over `workspace` to answer `question`, reporting into the
/// chat channel as it goes.
///
/// The loop speaks [`AgentEvent`]; the chat speaks [`ChatEvent`]. This is the adapter. Tool
/// calls stream in as progress lines ("reading starfield.rs") so the panel is not silent for
/// the many seconds a multi-step investigation takes — silence there is what makes a model
/// look hung. The `finish` argument is the answer, and becomes the reply.
fn investigate_into_chat(
    cfg: &UiConfig,
    question: &str,
    workspace: &std::path::Path,
    tx: &std::sync::mpsc::Sender<ChatEvent>,
) {
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let cfg2 = cfg.clone();
    let q = question.to_string();
    let ws = workspace.to_path_buf();
    // The loop is blocking; run it on its own thread so this one can drain events and keep
    // the panel updating while it works.
    // SAY WHAT IS ABOUT TO HAPPEN, before anything slow starts.
    //
    // An investigation takes minutes on a local model, and until the first tool call lands
    // the panel showed nothing at all -- the user asked a question and got silence. The plan
    // comes from the harness rather than the model on purpose: it is instant, it cannot be
    // truncated, and the model's own prompt forbids prose outside a tool call, so a model
    // written plan would be discarded by the parser anyway.
    let _ = tx.send(ChatEvent::Token(
        // One line, deliberately. A backslash-continued string here had its continuation
        // reflowed by rustfmt into LITERAL spaces, so the user saw "and come          back
        // with the". A single line cannot be reflowed and cannot pick up source indentation.
        INVESTIGATION_PLAN.to_string(),
    ));

    let worker = thread::spawn(move || {
        crate::session::agent::investigate(cfg2, q, ws, ev_tx);
    });

    let mut answer = String::new();
    let mut steps = 0usize;
    while let Ok(ev) = ev_rx.recv() {
        match ev {
            crate::session::UiEvent::Agent(a) => match a {
                sc_core::AgentEvent::ToolCall { tool, arg } => {
                    if tool == "finish" {
                        // The finish argument IS the answer.
                        answer = arg;
                    } else {
                        steps += 1;
                        // A progress line, not the reply: shown live, replaced by the answer.
                        let _ = tx.send(ChatEvent::Token(format!("· {tool} {arg}\n")));
                    }
                }
                sc_core::AgentEvent::HarnessFault { kind, detail, .. } => {
                    // Surface OUR faults in the chat rather than only in a log the user
                    // never opens — a truncated or over-budget turn is why an answer is
                    // thin, and staying silent about it blames the model.
                    let _ = tx.send(ChatEvent::Token(format!(
                        "\n⚠ harness fault ({}): {detail}\n",
                        kind.label()
                    )));
                }
                _ => {}
            },
            crate::session::UiEvent::Failed(msg) => {
                let _ = tx.send(ChatEvent::Failed(msg));
                let _ = worker.join();
                return;
            }
            crate::session::UiEvent::Done { .. } => break,
            _ => {}
        }
    }
    let _ = worker.join();

    let text = if answer.trim().is_empty() {
        // Reading without concluding. Say so plainly: an empty bubble after a visible
        // multi-step search reads as a crash.
        format!(
            "I read the code for this but did not reach a conclusion \
             (searched across {steps} step(s)). Try narrowing the question, or name the \
             file you think is involved."
        )
    } else {
        answer
    };
    let _ = tx.send(ChatEvent::Reply {
        text,
        intent: Some(ChatIntent::Question),
        truncated: false,
    });
}

#[cfg(test)]
mod plan_line {
    /// **A continued string literal can pick up its own source indentation.**
    ///
    /// The plan line was written with a `\`-continuation; rustfmt reflowed it and the
    /// continuation's leading whitespace became LITERAL spaces, so the panel rendered
    /// "and come          back with the file". Nothing in the type system catches text
    /// that is merely ugly, so this asserts it.
    #[test]
    fn the_plan_line_has_no_stray_whitespace() {
        let line = super::INVESTIGATION_PLAN;
        assert!(
            !line.contains("  "),
            "a run of spaces means a reflowed continuation leaked indentation: {line:?}"
        );
        assert!(line.ends_with("\n\n"), "the plan is a paragraph of its own");
        assert!(!line.trim().is_empty());
    }
}
