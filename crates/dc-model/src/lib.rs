//! Model Gateway — the single seam between `dumb-coder` and *any* inference
//! runtime (see spec 02-model-backends).
//!
//! Everything above this crate talks to the [`ModelBackend`] trait, never to a
//! concrete runtime. So far we ship three implementations:
//!
//! * [`CallbackBackend`] — the **integration seam** (spec 12): inference is an
//!   injected closure. On Android the closure is a JNI up-call into the Kotlin
//!   AICore wrapper; on the desktop/tests it's any local function. Fully testable
//!   on the host without a device.
//! * [`AndroidCoreBackend`] — the on-device target's *self-contained* form
//!   (AICore / LiteRT-LM, Gemma 4). It compiles everywhere but only *runs* on an
//!   Android device, so on other hosts it returns a clear, actionable error
//!   rather than pretending.
//! * [`MockBackend`] — a scriptable stand-in so the harness and tests run in
//!   CI / on a dev box where no device is present.
//! * [`OpenAiBackend`] — the **primary off-device path** (spec 02): any
//!   OpenAI-compatible HTTP server (Ollama compat, llama.cpp `--api`, vLLM, LM
//!   Studio). This is what lets the harness drive a real small model today.
//!
//! The trait is synchronous for now; streaming/async land with the real HTTP and
//! on-device adapters (M0/M8). The shape (capabilities, generate) matches spec 02.

mod constraint;
mod openai;
pub use constraint::{OutputConstraint, ToolCalling, ToolSchema};
pub use openai::OpenAiBackend;

use std::cell::RefCell;
use std::collections::VecDeque;

use dc_proto::{DcError, Result};

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
    /// Stable identifier for logs/reports (e.g. `"android-core"`, `"mock"`).
    fn name(&self) -> &str;
    /// Static description of what this backend supports.
    fn capabilities(&self) -> Capabilities;
    /// Produce a single assistant turn for the request.
    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse>;
    /// Exact token count for `text`, when the backend has a tokenizer (spec 02).
    /// `None` means "no exact count available" — the Context Manager then falls
    /// back to a heuristic estimator with a safety margin (spec 05). Defaulted so
    /// existing backends opt in only when they truly have a tokenizer.
    fn count_tokens(&self, _text: &str) -> Option<usize> {
        None
    }
}

/// The primary on-device target: Android AICore (Gemma 4 as Gemini Nano 4) with
/// a self-hosted LiteRT-LM fallback (spec 02 / spec 10).
///
/// It compiles on every platform, but actually invoking the on-device runtime
/// requires an Android device — so on any other host `generate` returns a
/// `Backend` error explaining the situation instead of silently substituting a
/// different model. Wiring it to the real runtime is M8 work.
pub struct AndroidCoreBackend {
    name: String,
    /// Which on-device path we'd use on a real device.
    pub flavor: AndroidFlavor,
}

/// The two on-device Android flavors (spec 02 — On-device / Android).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndroidFlavor {
    /// OS-managed model via AICore (Gemma 4 / Gemini Nano 4); flagship devices.
    AiCore,
    /// Self-hosted Gemma 4 E4B/E2B via the LiteRT-LM runtime; broad devices.
    LiteRtLm,
}

impl AndroidCoreBackend {
    /// Default posture: prefer AICore where present (spec 02).
    pub fn new() -> Self {
        Self {
            name: "android-core".to_string(),
            flavor: AndroidFlavor::AiCore,
        }
    }

    pub fn with_flavor(flavor: AndroidFlavor) -> Self {
        Self {
            name: "android-core".to_string(),
            flavor,
        }
    }

    /// True only when running on an Android device with the runtime available.
    /// On every other host this is false (M8 will implement the real probe).
    pub fn is_available(&self) -> bool {
        cfg!(target_os = "android")
    }
}

impl Default for AndroidCoreBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelBackend for AndroidCoreBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Gemma 4 E4B advertises 128K; we budget against an effective
            // fraction elsewhere (spec 05).
            max_context_tokens: 128_000,
            // Gemma 4 has native function-calling (spec 02).
            tool_calling: ToolCalling::OpenAiStyle,
            on_device: true,
        }
    }

    fn generate(&self, _req: &GenerateRequest) -> Result<GenerateResponse> {
        if !self.is_available() {
            return Err(DcError::Backend(format!(
                "android-core ({:?}) requires an Android device; not available on this host. \
                 Use a runnable backend (e.g. MockBackend in tests, or Ollama/OpenAI-compat) \
                 to execute the harness off-device. On-device wiring is M8.",
                self.flavor
            )));
        }
        // On a real device this would call into AICore / LiteRT-LM.
        Err(DcError::Backend(
            "android-core on-device generation not yet implemented (M8)".to_string(),
        ))
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
/// This is the **integration seam for the on-device Android target** (spec 12):
/// the Rust agent core stays platform-agnostic, and the actual inference is
/// supplied from outside. On Android, the closure performs a JNI up-call into the
/// Kotlin ML Kit GenAI / AICore wrapper and returns the generated text; in tests
/// and on the desktop it can be any local function. Because the closure is just
/// `Fn(&GenerateRequest) -> Result<GenerateResponse>`, the whole contract is
/// exercisable on the host without an Android device.
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

    /// Convenience constructor matching the Android-core capability profile
    /// (on-device, native tool-calling, Gemma 4's 128K window).
    pub fn android_core(generate: F) -> Self {
        Self::new(
            "android-core",
            Capabilities {
                max_context_tokens: 128_000,
                tool_calling: ToolCalling::OpenAiStyle,
                on_device: true,
            },
            generate,
        )
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

    #[test]
    fn android_core_is_unavailable_off_device_with_actionable_error() {
        let backend = AndroidCoreBackend::new();
        // This crate is built/tested on a non-Android host.
        assert!(!backend.is_available());

        let req = GenerateRequest::new(vec![Message::user("hi")]);
        let err = backend.generate(&req).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires an Android device"), "got: {msg}");
    }

    #[test]
    fn android_core_advertises_on_device_and_large_context() {
        let caps = AndroidCoreBackend::new().capabilities();
        assert!(caps.on_device);
        assert_eq!(caps.tool_calling, ToolCalling::OpenAiStyle);
        assert_eq!(caps.max_context_tokens, 128_000);
    }

    #[test]
    fn callback_backend_delegates_to_the_injected_closure() {
        // Stands in for the Android JNI up-call into Kotlin/AICore: here the
        // "model" just echoes the last user message in upper case.
        let backend = CallbackBackend::android_core(|req: &GenerateRequest| {
            let last = req
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(GenerateResponse {
                content: last.to_uppercase(),
            })
        });

        assert_eq!(backend.name(), "android-core");
        assert!(backend.capabilities().on_device);

        let req = GenerateRequest::new(vec![Message::user("ping")]);
        assert_eq!(backend.generate(&req).unwrap().content, "PING");
    }

    #[test]
    fn callback_backend_propagates_errors_from_the_closure() {
        // Models the AICore feature being unavailable / download pending.
        let backend = CallbackBackend::android_core(|_req: &GenerateRequest| {
            Err(DcError::Backend("AICore feature not yet downloaded".into()))
        });
        let req = GenerateRequest::new(vec![Message::user("hi")]);
        assert!(backend.generate(&req).is_err());
    }

    #[test]
    fn callback_backend_is_usable_through_the_trait_object() {
        // Confirms the seam works behind `&dyn ModelBackend`, which is how the
        // agent loop will hold whatever backend it's handed.
        let backend = CallbackBackend::android_core(|_r| {
            Ok(GenerateResponse {
                content: "ok".into(),
            })
        });
        let dynamic: &dyn ModelBackend = &backend;
        let req = GenerateRequest::new(vec![Message::user("x")]);
        assert_eq!(dynamic.generate(&req).unwrap().content, "ok");
    }
}
