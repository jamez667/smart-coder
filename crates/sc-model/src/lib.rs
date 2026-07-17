//! Model Gateway — the single seam between `smart-coder` and *any* inference
//! runtime (see spec 02-model-backends).
//!
//! Everything above this crate talks to the [`ModelBackend`] trait, never to a
//! concrete runtime. So far we ship three implementations:
//!
//! * [`CallbackBackend`] — a general **integration seam**: inference is an
//!   injected closure (a JNI up-call, an HTTP client, or a canned test function).
//!   Fully testable on the host with no live model.
//! * [`MockBackend`] — a scriptable stand-in so the harness and tests run in
//!   CI / on a dev box where no model is present.
//! * [`OpenAiBackend`] — the **primary path** (spec 02): any OpenAI-compatible
//!   HTTP server (Ollama compat, llama.cpp `--api`, vLLM, LM Studio). This is what
//!   lets the harness drive a real small model today.
//!
//! The trait is synchronous for now; streaming/async land with the real HTTP
//! adapters. The shape (capabilities, generate) matches spec 02.

mod constraint;
mod openai;
pub use constraint::{OutputConstraint, ToolCalling, ToolSchema};
pub use openai::OpenAiBackend;

use std::cell::RefCell;
use std::collections::VecDeque;

use sc_proto::{DcError, Result};

/// Role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A single chat message.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// One generation request. Sampling is pinned per call so sessions are
/// reproducible (spec 03 — determinism & replay).
#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub messages: Vec<Message>,
    pub max_tokens: usize,
    pub temperature: f32,
    /// Optional structural enforcement on the output (spec 02). A capability-aware
    /// strategy sets this; a backend applies the variant it supports and ignores
    /// the rest. `None` means plain completion (prompt + parse + repair).
    pub constraint: Option<OutputConstraint>,
}

impl GenerateRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            max_tokens: 1024,
            temperature: 0.2,
            constraint: None,
        }
    }

    /// Attach an output constraint (builder style).
    pub fn with_constraint(mut self, constraint: OutputConstraint) -> Self {
        self.constraint = Some(constraint);
        self
    }
}

/// One generation result.
#[derive(Debug, Clone)]
pub struct GenerateResponse {
    pub content: String,
}

/// What a backend can do, negotiated at runtime (spec 02 — capabilities).
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub max_context_tokens: usize,
    /// How (if at all) the backend can *enforce* a well-formed tool call — the
    /// single most important capability for small-model reliability (spec 02).
    pub tool_calling: ToolCalling,
    pub on_device: bool,
}

/// The one trait every inference runtime implements.
pub trait ModelBackend {
    /// Stable identifier for logs/reports (e.g. `"openai"`, `"mock"`).
    fn name(&self) -> &str;
    /// Static description of what this backend supports.
    fn capabilities(&self) -> Capabilities;
    /// Produce a single assistant turn for the request.
    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse>;
    /// Like [`generate`], but invokes `on_token` with each content delta as it is produced
    /// (for a live "watch it type" view). The default falls back to a blocking `generate`
    /// and delivers the whole result as one delta — so a backend that can't stream still
    /// works, just without the incremental view. A real HTTP backend overrides this with SSE.
    ///
    /// [`generate`]: ModelBackend::generate
    fn generate_streaming(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<GenerateResponse> {
        let resp = self.generate(req)?;
        on_token(&resp.content);
        Ok(resp)
    }
    /// Exact token count for `text`, when the backend has a tokenizer (spec 02).
    /// `None` means "no exact count available" — the Context Manager then falls
    /// back to a heuristic estimator with a safety margin (spec 05). Defaulted so
    /// existing backends opt in only when they truly have a tokenizer.
    fn count_tokens(&self, _text: &str) -> Option<usize> {
        None
    }
}

/// A scriptable backend for tests and off-device harness runs.
///
/// Hand it a queue of canned responses; each `generate` pops the next one. When
/// the script is exhausted it errors, which keeps tests honest about how many
/// model turns they expect.
pub struct MockBackend {
    name: String,
    responses: RefCell<VecDeque<String>>,
    caps: Capabilities,
}

impl MockBackend {
    /// Build a mock that will emit `responses` in order.
    pub fn new<I, S>(responses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            name: "mock".to_string(),
            responses: RefCell::new(responses.into_iter().map(Into::into).collect()),
            caps: Capabilities {
                max_context_tokens: 8_192,
                tool_calling: ToolCalling::None,
                on_device: false,
            },
        }
    }

    /// Number of scripted responses not yet consumed.
    pub fn remaining(&self) -> usize {
        self.responses.borrow().len()
    }
}

impl ModelBackend for MockBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn generate(&self, _req: &GenerateRequest) -> Result<GenerateResponse> {
        match self.responses.borrow_mut().pop_front() {
            Some(content) => Ok(GenerateResponse { content }),
            None => Err(DcError::Backend(
                "mock backend script exhausted (more generate() calls than scripted responses)"
                    .to_string(),
            )),
        }
    }
}

/// A backend whose generation is delegated to an injected closure.
///
/// A general integration seam: the Rust agent core stays runtime-agnostic and the
/// actual inference is supplied from outside — a JNI up-call, an HTTP client, or a
/// canned function in tests. Because the closure is just
/// `Fn(&GenerateRequest) -> Result<GenerateResponse>`, the whole contract is
/// exercisable on the host with no live model.
pub struct CallbackBackend<F> {
    name: String,
    caps: Capabilities,
    generate: F,
}

impl<F> CallbackBackend<F>
where
    F: Fn(&GenerateRequest) -> Result<GenerateResponse>,
{
    /// Build a callback backend with the given name, capabilities, and closure.
    pub fn new(name: impl Into<String>, caps: Capabilities, generate: F) -> Self {
        Self {
            name: name.into(),
            caps,
            generate,
        }
    }

}

impl<F> ModelBackend for CallbackBackend<F>
where
    F: Fn(&GenerateRequest) -> Result<GenerateResponse>,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        (self.generate)(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_emits_scripted_responses_in_order() {
        let backend = MockBackend::new(["first", "second"]);
        let req = GenerateRequest::new(vec![Message::user("hi")]);

        assert_eq!(backend.remaining(), 2);
        assert_eq!(backend.generate(&req).unwrap().content, "first");
        assert_eq!(backend.generate(&req).unwrap().content, "second");
        assert_eq!(backend.remaining(), 0);
    }

    #[test]
    fn mock_errors_when_script_exhausted() {
        let backend = MockBackend::new(Vec::<String>::new());
        let req = GenerateRequest::new(vec![Message::user("hi")]);
        assert!(backend.generate(&req).is_err());
    }

    /// A capability profile for the callback-seam tests below.
    fn seam_caps() -> Capabilities {
        Capabilities {
            max_context_tokens: 128_000,
            tool_calling: ToolCalling::OpenAiStyle,
            on_device: false,
        }
    }

    #[test]
    fn callback_backend_delegates_to_the_injected_closure() {
        // The "model" just echoes the last user message in upper case.
        let backend = CallbackBackend::new("echo", seam_caps(), |req: &GenerateRequest| {
            let last = req
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(GenerateResponse {
                content: last.to_uppercase(),
            })
        });

        assert_eq!(backend.name(), "echo");

        let req = GenerateRequest::new(vec![Message::user("ping")]);
        assert_eq!(backend.generate(&req).unwrap().content, "PING");
    }

    #[test]
    fn callback_backend_propagates_errors_from_the_closure() {
        let backend = CallbackBackend::new("erroring", seam_caps(), |_req: &GenerateRequest| {
            Err(DcError::Backend("backend unavailable".into()))
        });
        let req = GenerateRequest::new(vec![Message::user("hi")]);
        assert!(backend.generate(&req).is_err());
    }

    #[test]
    fn callback_backend_is_usable_through_the_trait_object() {
        // Confirms the seam works behind `&dyn ModelBackend`, which is how the
        // agent loop holds whatever backend it's handed.
        let backend = CallbackBackend::new("ok", seam_caps(), |_r| {
            Ok(GenerateResponse {
                content: "ok".into(),
            })
        });
        let dynamic: &dyn ModelBackend = &backend;
        let req = GenerateRequest::new(vec![Message::user("x")]);
        assert_eq!(dynamic.generate(&req).unwrap().content, "ok");
    }
}
