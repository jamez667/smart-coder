//! Replay backend — re-run a recorded session with no model (spec 03, "Determinism &
//! replay").
//!
//! A harness change is only safe if the loop still does the same thing given the same
//! model replies. [`ReplayBackend`] makes that a plain test: hand it the replies a real
//! model gave (from a session log or a transcript), drive the loop, and assert the
//! tool-call sequence matches what was recorded. No inference, no network, no VRAM —
//! so it runs in CI and on a laptop.
//!
//! It also keeps every prompt it was shown ([`ReplayBackend::prompts`]) so a test can
//! check that turn N's prompt is a prefix-stable extension of turn N-1's — the property
//! that makes KV-cache reuse work and that a careless prompt-builder change silently
//! breaks.
//!
//! Two recorded shapes are understood, parsed by field name so this crate needs no
//! dependency on the crates that write them:
//!
//! * the NDJSON session log `sc_core::JsonLinesSink` writes — one tagged `AgentEvent`
//!   per line; the reply is `{"type":"ModelTurn", ..., "raw": "..."}`;
//! * the `transcript-*.jsonl` this crate's own [`crate::transcript`] writes — one
//!   backend call per line; the reply is the `reply` field of an `ok: true` entry
//!   whose `call` is `generate` or `generate_streaming`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;

use sc_proto::{DcError, Result};

use crate::{Capabilities, GenerateRequest, GenerateResponse, Message, ModelBackend, ToolCalling};

/// A backend that replays a recorded queue of model replies, in order, and records
/// the prompts it was asked for.
///
/// Like [`crate::MockBackend`], it errors once the queue is empty — a replayed run
/// that asks for one more turn than the recording had is a behaviour change, and the
/// error says so with the count.
#[derive(Debug)]
pub struct ReplayBackend {
    name: String,
    replies: RefCell<VecDeque<String>>,
    /// How many replies have been handed out — the "N" in the exhausted message.
    served: Cell<usize>,
    prompts: RefCell<Vec<Vec<Message>>>,
    caps: Capabilities,
}

impl ReplayBackend {
    /// Replay `replies` verbatim, in order.
    pub fn from_replies(replies: Vec<String>) -> Self {
        Self {
            name: "replay".to_string(),
            replies: RefCell::new(replies.into()),
            served: Cell::new(0),
            prompts: RefCell::new(Vec::new()),
            caps: Capabilities {
                max_context_tokens: 32_768,
                tool_calling: ToolCalling::None,
                on_device: false,
            },
        }
    }

    /// Replay the `ModelTurn` replies from a session log written by
    /// `sc_core::JsonLinesSink` (one JSON-tagged `AgentEvent` per line). Lines that are
    /// blank, not JSON, or any other event type are skipped.
    pub fn from_ndjson(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(DcError::Io)?;
        Ok(Self::from_replies(replies_from_ndjson(&text)))
    }

    /// Replay the successful `generate` / `generate_streaming` replies from a
    /// `transcript-*.jsonl` written by [`crate::transcript`]. Failed calls and other
    /// call kinds (e.g. a health probe) are skipped.
    pub fn from_transcript(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(DcError::Io)?;
        Ok(Self::from_replies(replies_from_transcript(&text)))
    }

    /// Override the advertised capabilities (default: 32768-token context, no native
    /// tool calling). Match them to the backend that made the recording, or the loop's
    /// prompt budget — and so its choices — will differ from the recorded run.
    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Number of recorded replies not yet consumed.
    pub fn remaining(&self) -> usize {
        self.replies.borrow().len()
    }

    /// Every prompt this backend has been asked to answer, in call order — one
    /// message list per `generate`, exactly as the loop sent it.
    pub fn prompts(&self) -> Vec<Vec<Message>> {
        self.prompts.borrow().clone()
    }
}

/// Pull the model replies out of NDJSON session-log text (pure, for the tests).
fn replies_from_ndjson(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("ModelTurn"))
        .filter_map(|v| v.get("raw").and_then(|r| r.as_str()).map(str::to_string))
        .collect()
}

/// Pull the successful generation replies out of transcript-log text (pure, for the
/// tests).
fn replies_from_transcript(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .filter(|v| v.get("ok").and_then(|o| o.as_bool()) == Some(true))
        .filter(|v| {
            matches!(
                v.get("call").and_then(|c| c.as_str()),
                Some("generate") | Some("generate_streaming")
            )
        })
        .filter_map(|v| v.get("reply").and_then(|r| r.as_str()).map(str::to_string))
        .collect()
}

impl ModelBackend for ReplayBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        self.prompts.borrow_mut().push(req.messages.clone());
        match self.replies.borrow_mut().pop_front() {
            Some(content) => {
                self.served.set(self.served.get() + 1);
                Ok(GenerateResponse::new(content))
            }
            None => Err(DcError::Backend(format!(
                "replay exhausted after {} turns (the loop asked for one more model turn than the recording has)",
                self.served.get()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(user: &str) -> GenerateRequest {
        GenerateRequest::new(vec![Message::system("sys"), Message::user(user)])
    }

    /// Write `text` to a fresh temp file and return its path.
    fn temp_file(tag: &str, text: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sc-model-replay-{tag}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn from_replies_serves_in_order_then_reports_exhaustion_with_the_count() {
        let b = ReplayBackend::from_replies(vec!["one".into(), "two".into()]);
        assert_eq!(b.remaining(), 2);
        assert_eq!(b.generate(&req("a")).unwrap().content, "one");
        assert_eq!(b.generate(&req("b")).unwrap().content, "two");
        assert_eq!(b.remaining(), 0);

        let err = b.generate(&req("c")).unwrap_err().to_string();
        assert!(
            err.contains("replay exhausted after 2 turns"),
            "the error must say how far the recording went, got: {err}"
        );
    }

    #[test]
    fn prompts_are_recorded_in_call_order() {
        let b = ReplayBackend::from_replies(vec!["x".into(), "y".into()]);
        b.generate(&req("first")).unwrap();
        b.generate(&req("second")).unwrap();
        // The exhausted call is still a prompt the loop sent; it is recorded too.
        let _ = b.generate(&req("third"));

        let prompts = b.prompts();
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts[0][1].content, "first");
        assert_eq!(prompts[1][1].content, "second");
        assert_eq!(prompts[2][1].content, "third");
        // The system message is carried through verbatim — that's what a
        // prefix-stability assertion will compare.
        assert_eq!(prompts[0][0].content, "sys");
    }

    #[test]
    fn capabilities_default_and_override() {
        let b = ReplayBackend::from_replies(vec![]);
        let caps = b.capabilities();
        assert_eq!(caps.max_context_tokens, 32_768);
        assert_eq!(caps.tool_calling, ToolCalling::None);

        let b = b.with_capabilities(Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::OpenAiStyle,
            on_device: true,
        });
        let caps = b.capabilities();
        assert_eq!(caps.max_context_tokens, 8_192);
        assert_eq!(caps.tool_calling, ToolCalling::OpenAiStyle);
        assert!(caps.on_device);
    }

    #[test]
    fn from_ndjson_takes_model_turn_raw_and_ignores_the_rest() {
        // The exact shape JsonLinesSink writes: tagged objects, one per line, with
        // other event kinds interleaved and a blank/garbage line for tolerance.
        let text = concat!(
            r#"{"type":"RunStarted","task":"do it","prompt_budget":5120}"#,
            "\n",
            r#"{"type":"ModelTurn","step":1,"prompt_tokens":10,"raw":"{\"tool\":\"read_file\",\"path\":\"a.txt\"}"}"#,
            "\n",
            r#"{"type":"ToolCall","tool":"read_file","arg":"a.txt"}"#,
            "\n",
            "\n",
            "not json at all\n",
            r#"{"type":"SomeFutureEvent","raw":"must not be picked up"}"#,
            "\n",
            r#"{"type":"ModelTurn","step":2,"prompt_tokens":20,"raw":"{\"tool\":\"finish\"}"}"#,
            "\n",
            r#"{"type":"Stopped","reason":"Finished"}"#,
            "\n",
        );
        let path = temp_file("ndjson", text);
        let b = ReplayBackend::from_ndjson(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(b.remaining(), 2);
        assert_eq!(
            b.generate(&req("1")).unwrap().content,
            r#"{"tool":"read_file","path":"a.txt"}"#
        );
        assert_eq!(
            b.generate(&req("2")).unwrap().content,
            r#"{"tool":"finish"}"#
        );
    }

    #[test]
    fn from_transcript_takes_ok_generate_replies_only() {
        // The exact shape transcript::record produces. Includes a failed call, a
        // non-generation call kind, and a streaming call that must be kept.
        let text = concat!(
            r#"{"ts_ms":1,"call":"generate","model":"m","endpoint":"e","temperature":0.2,"max_tokens":8,"constraint":null,"messages":[{"role":"user","content":"hi"}],"ok":true,"reply":"first","error":null,"ms":5}"#,
            "\n",
            r#"{"ts_ms":2,"call":"generate","model":"m","endpoint":"e","temperature":0.2,"max_tokens":8,"constraint":null,"messages":[],"ok":false,"reply":null,"error":"connection refused","ms":5}"#,
            "\n",
            r#"{"ts_ms":3,"call":"probe","model":"m","endpoint":"e","temperature":0.0,"max_tokens":1,"constraint":null,"messages":[],"ok":true,"reply":"pong","error":null,"ms":1}"#,
            "\n",
            r#"{"ts_ms":4,"call":"generate_streaming","model":"m","endpoint":"e","temperature":0.2,"max_tokens":8,"constraint":null,"messages":[],"ok":true,"reply":"second","error":null,"ms":5}"#,
            "\n",
        );
        let path = temp_file("transcript", text);
        let b = ReplayBackend::from_transcript(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(b.remaining(), 2);
        assert_eq!(b.generate(&req("1")).unwrap().content, "first");
        assert_eq!(b.generate(&req("2")).unwrap().content, "second");
    }

    #[test]
    fn a_missing_file_is_an_io_error_not_a_panic() {
        let err = ReplayBackend::from_ndjson("definitely/not/here.ndjson").unwrap_err();
        assert!(matches!(err, DcError::Io(_)), "got {err:?}");
        let err = ReplayBackend::from_transcript("definitely/not/here.jsonl").unwrap_err();
        assert!(matches!(err, DcError::Io(_)), "got {err:?}");
    }
}
