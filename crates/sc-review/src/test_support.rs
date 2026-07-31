//! Scripted backends for the crate's own tests (the `Scripted` pattern from
//! `sc-core/tests/tdd_loop.rs`). No test in this crate needs a live model: the
//! lens prompts, corroboration, ranking, anchor matching and vote merging are all
//! pure logic over fixtures (spec 16 — "How to work").

use std::sync::Mutex;

use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::{DcError, Result};

/// A backend that answers by matching a substring of the prompt it was given —
/// so one instance can play every lens, keyed on each lens's question.
pub struct ScriptedReviewer {
    name: String,
    replies: Vec<(String, String)>,
    /// Prompts seen, in order — so a test can assert what the reviewer was
    /// actually shown (that grounding reached it, say).
    pub seen: Mutex<Vec<String>>,
}

impl ScriptedReviewer {
    pub fn new(name: &str, replies: Vec<(&str, &str)>) -> Self {
        Self {
            name: name.to_string(),
            replies: replies
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl ModelBackend for ScriptedReviewer {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 32_768,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        let prompt = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.seen.lock().unwrap().push(prompt.clone());
        for (key, reply) in &self.replies {
            if prompt.contains(key.as_str()) {
                return Ok(GenerateResponse {
                    content: reply.clone(),
                });
            }
        }
        // Unscripted: found nothing. The common case for a real reviewer.
        Ok(GenerateResponse {
            content: "[]".to_string(),
        })
    }
}

/// A reviewer scripted on prompt substrings, named `scripted`.
pub fn scripted(replies: Vec<(&str, &str)>) -> ScriptedReviewer {
    ScriptedReviewer::new("scripted", replies)
}

/// A backend that cannot be reached — an API outage, a dead endpoint.
pub struct FailingBackend;

impl ModelBackend for FailingBackend {
    fn name(&self) -> &str {
        "unreachable"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _req: &GenerateRequest) -> Result<GenerateResponse> {
        Err(DcError::Backend("connection refused".to_string()))
    }
}

pub fn failing_backend() -> FailingBackend {
    FailingBackend
}

/// A scratch workspace directory for tests that need a real repo to index.
pub fn temp_repo(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-review-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
