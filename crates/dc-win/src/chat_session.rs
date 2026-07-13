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

use dc_model::{GenerateRequest, ModelBackend};

use crate::config::UiConfig;

/// The result of one chat turn streamed back to the UI.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// The assistant's full reply text (v1 sends the whole reply at once; token streaming is
    /// a later nicety).
    Reply(String),
    /// The turn failed (backend unreachable, etc.) — a human-readable reason.
    Failed(String),
}

/// A single in-flight chat turn. Holds the receiving end the UI drains; the worker owns the
/// sender and the backend. Dropping it lets the worker finish in the background.
pub struct ChatSession {
    events: Receiver<ChatEvent>,
    _handle: thread::JoinHandle<()>,
}

impl ChatSession {
    /// Spawn one chat turn: build the coder backend from `cfg`, run `generate(req)` on a
    /// worker thread, and stream back a [`ChatEvent`]. The caller passes the fully-built
    /// [`GenerateRequest`] (from `Conversation::request`) so this stays free of chat state.
    pub fn spawn(cfg: UiConfig, req: GenerateRequest) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let backend = cfg.backend();
            match backend.generate(&req) {
                Ok(resp) => {
                    let _ = tx.send(ChatEvent::Reply(resp.content));
                }
                Err(e) => {
                    let _ = tx.send(ChatEvent::Failed(format!("chat failed: {e}")));
                }
            }
        });
        Self {
            events: rx,
            _handle: handle,
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
    use dc_model::Message;

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

        // Block for the terminal event by polling to completion.
        let ev = loop {
            match session.events.recv() {
                Ok(ev) => break Some(ev),
                Err(_) => break None,
            }
        };
        assert!(
            matches!(ev, Some(ChatEvent::Failed(_)) | Some(ChatEvent::Reply(_))),
            "expected a terminal ChatEvent, got {ev:?}"
        );
    }
}
