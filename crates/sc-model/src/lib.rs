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
mod dotenv;
mod openai;
pub mod replay;
pub mod transcript;
pub use constraint::{OutputConstraint, ToolCalling, ToolSchema};
pub use dotenv::load_dotenv;
pub use openai::OpenAiBackend;
pub use replay::ReplayBackend;

use std::cell::RefCell;
use std::collections::VecDeque;

use sc_proto::{DcError, Result};

/// Role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    /// The RESULT of a tool call, paired to the assistant turn that made it by
    /// [`Message::tool_call_id`].
    ///
    /// Only produced when the assistant turn it answers carries a native
    /// [`ToolCallRecord`]; a text-only turn keeps its observation as an ordinary
    /// `User` message, exactly as before. See [`Message::tool`].
    Tool,
}

/// One native tool call an assistant turn made, kept in the shape the backend
/// handed it over so the NEXT request can replay it faithfully.
///
/// **This is the fix for a whole class of silent corruption.** A model whose chat
/// template wraps calls in its own markup (`<tool_call>…</tool_call><|im_end|>` for
/// the ChatML family, Mellum2 included) only ever sees that markup if the call goes
/// back out as a structured `tool_calls` array — the template writes the wrapper.
/// Flattening the call to a bare `{"tool":…}` string in `content` shows the model a
/// dozen turns of history in a format its own template never emits; it imitates the
/// history and never produces the stop token, because the token that ends its turn
/// is part of the wrapper it was never shown. Measured on Mellum2-12B: 24 completion
/// tokens and a clean stop with a faithful history, 3,072 (the cap) and
/// `{"tool":"finish"}</tool_call>` repeated ~60 times with the flattened one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// The server's id for this call. Echoed back as the assistant message's
    /// `tool_calls[].id` and as the matching result's `tool_call_id`. Empty when the
    /// server did not supply one (llama.cpp often does not) — the wire builder then
    /// synthesises a stable one so the pair still matches.
    pub id: String,
    /// The function name, verbatim.
    pub name: String,
    /// The JSON-encoded argument object, verbatim — a *string* per the OpenAI
    /// schema, kept byte-for-byte rather than re-serialised so the replayed request
    /// is the same bytes the server sent.
    pub arguments: String,
}

impl ToolCallRecord {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Whether this call is safe to put on the wire: its `arguments` must parse as
    /// JSON.
    ///
    /// **A truncated reply is a valid tool call and an invalid JSON string at the same
    /// time.** When the model's output is cut off at the token cap mid-`write_file`, the
    /// harness's truncation salvage (`repair_truncated_file_write`) still recovers usable
    /// work from the partial body — but the `arguments` the server handed over are
    /// literally unterminated: a `"content"` string opened and never closed, because the
    /// bytes that would have closed it never arrived.
    /// Replaying those bytes verbatim inside a `tool_calls` array makes the NEXT request
    /// unparseable, and llama.cpp answers the whole thing with an HTTP 500 — which kills
    /// the task outright, salvaged work and all. Observed on `engine-diagonal-path`:
    /// `Failed to parse tool call arguments as JSON ... missing closing quote`, 0 steps.
    ///
    /// This is about VALIDITY, not size. A huge but well-formed argument object is fine
    /// and goes out natively; a short malformed one does not.
    pub fn is_replayable(&self) -> bool {
        serde_json::from_str::<serde_json::Value>(&self.arguments).is_ok()
    }

    /// The id this call is replayed under: the server's own when it gave one,
    /// otherwise a deterministic stand-in derived from the call itself.
    ///
    /// **One implementation, because two halves have to agree.** The assistant
    /// message's `tool_calls[].id` and its result's `tool_call_id` are what a chat
    /// template matches a call to its answer by; if the harness and the wire builder
    /// each invented their own id, the pair would silently come apart on every server
    /// that does not supply ids (llama.cpp usually does not).
    pub fn wire_id(&self) -> String {
        if !self.id.is_empty() {
            return self.id.clone();
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.name.hash(&mut h);
        self.arguments.hash(&mut h);
        format!("call_{:016x}", h.finish())
    }
}

/// A single chat message.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// The native tool calls this ASSISTANT turn made, when the backend returned a
    /// structured `tool_calls` array. Empty for every other message and for a turn
    /// decoded by the `ParseRepair`/`Grammar` strategies, whose output is genuinely
    /// text — those replay byte-identically to how they always have.
    pub tool_calls: Vec<ToolCallRecord>,
    /// For a [`Role::Tool`] message, the id of the call it answers. `None` elsewhere.
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }

    /// A tool RESULT, paired to the call `id` it answers.
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }

    /// An assistant turn that made native tool calls: the normalised text form the
    /// harness's extractor reads, PLUS the structured calls the wire builder replays.
    pub fn assistant_with_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallRecord>,
    ) -> Self {
        // THE GUARD. A call whose `arguments` is not valid JSON never reaches the wire.
        // If ANY call on this turn is malformed the whole turn falls back to the pre-fix
        // shape — plain assistant content, no `tool_calls` field — which is what the
        // harness did for its entire life before the native round trip and which
        // demonstrably survives. All-or-nothing rather than per-call, because the
        // observation pairs to `tool_calls[0]`: dropping just the bad one would silently
        // re-point the result at a call that never ran.
        //
        // The content is unaffected either way: it is the harness's normalised
        // `{"tool":…}` text, which every extractor above already reads, so the model
        // still sees what it did — just as prose rather than as a structured call.
        let replayable = tool_calls.iter().all(ToolCallRecord::is_replayable);
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: if replayable { tool_calls } else { Vec::new() },
            tool_call_id: None,
        }
    }

    /// True when this turn carries a native call to replay (so its observation must
    /// go back as a paired `tool` result rather than a plain user message).
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
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
    /// Sampling seed, when the caller wants a reproducible draw (spec 02/03). `None`
    /// leaves it to the server; a backend that honours it sends it as `seed`.
    pub seed: Option<u64>,
    /// Stop sequences: generation ends when the model emits any of these (spec 02).
    /// Empty means none; a backend sends them as `stop` only when set.
    pub stop: Vec<String>,
}

impl GenerateRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            max_tokens: 1024,
            temperature: 0.2,
            constraint: None,
            seed: None,
            stop: Vec::new(),
        }
    }

    /// Attach an output constraint (builder style).
    pub fn with_constraint(mut self, constraint: OutputConstraint) -> Self {
        self.constraint = Some(constraint);
        self
    }

    /// Pin the sampling seed (builder style).
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Set the stop sequences (builder style).
    pub fn with_stop<I, S>(mut self, stop: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.stop = stop.into_iter().map(Into::into).collect();
        self
    }
}

/// One generation result.
#[derive(Debug, Clone, Default)]
pub struct GenerateResponse {
    pub content: String,
    /// Why the server stopped generating, verbatim, when it says so.
    ///
    /// **Without this the harness cannot tell "the model finished" from "we cut it
    /// off".** A reply truncated at `max_tokens` arrives as ordinary content with no
    /// tool call in it, which reads exactly like a model that declined to act — that
    /// confusion cost 54 dead turns on one SWE-bench instance and took a transcript
    /// dig to find. With it, the loop can say so on the first turn.
    ///
    /// `None` means *unknown*, never "not truncated": backends that do not report it
    /// and the streaming path both leave it unset, so a detector must not read `None`
    /// as healthy. `Some("length")` is the truncation case.
    ///
    /// Defaulted so the many `GenerateResponse::new(content)` literals across the
    /// workspace's test mocks keep compiling — only a backend that actually learns the
    /// reason needs to set it.
    #[doc(hidden)]
    pub finish_reason: Option<String>,
    /// How many tokens the server counted in the PROMPT, when it reports `usage`.
    ///
    /// The one number that checks the harness's own accounting against the
    /// tokenizer that actually ran: the context builder's `tokens_used` should land
    /// within a few percent of this, and a gap is the counter being wrong, not the
    /// model. `None` when the server does not say (mocks, and servers without
    /// `usage`).
    pub prompt_tokens: Option<usize>,
    /// How many of those prompt tokens the server served from its KV cache
    /// instead of re-evaluating (llama.cpp's `timings.cache_n`, or OpenAI's
    /// `usage.prompt_tokens_details.cached_tokens`).
    ///
    /// **This is the number that makes an append-only prompt visible.**
    /// `prompt_tokens` counts what the harness SENDS, so keeping the prefix
    /// byte-stable between turns cannot move it at all -- the work saved happens
    /// on the server, in what it does NOT have to prefill. A stable prefix shows
    /// up here as a cached count that grows with the conversation; a prompt whose
    /// prefix shifts shows up as a cached count near zero every turn.
    ///
    /// `None` when the server does not report it (mocks, and every non-llama.cpp
    /// server without `prompt_tokens_details`).
    pub cached_prompt_tokens: Option<usize>,
    /// How many prompt tokens the server actually PREFILLED this turn
    /// (llama.cpp's `timings.prompt_n`); derived as `prompt_tokens - cached` when
    /// only the OpenAI-shaped `prompt_tokens_details` is available.
    ///
    /// The other half of the ratio: `prefilled + cached` is the whole prompt, and
    /// `prefilled` alone is the compute the turn actually cost.
    pub prefilled_prompt_tokens: Option<usize>,
    /// Milliseconds the server spent on the prefill (llama.cpp's
    /// `timings.prompt_ms`) -- the wall-clock consequence of the two counts above.
    pub prompt_ms: Option<f64>,
    /// The NATIVE tool calls this reply carried, when the backend returned a
    /// structured `tool_calls` array.
    ///
    /// [`Self::content`] still holds the harness's normalised `{"tool":…}` text form
    /// (every extractor reads that, and nothing about it changes). This is the same
    /// call kept in the shape the SERVER sent, so the loop can store it on the
    /// assistant turn and replay it faithfully next turn instead of showing the model
    /// a flattened string its own chat template never emits — see [`ToolCallRecord`].
    ///
    /// Empty for a plain-completion or grammar reply, whose output is genuinely text.
    pub tool_calls: Vec<ToolCallRecord>,
}

impl GenerateResponse {
    /// A response carrying only content.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Default::default()
        }
    }

    /// The same, plus the server's stop reason.
    pub fn with_finish_reason(content: impl Into<String>, reason: Option<String>) -> Self {
        Self {
            content: content.into(),
            finish_reason: reason,
            ..Default::default()
        }
    }

    /// What fraction of this turn's prompt the server served from cache, as a
    /// whole percent. `None` unless the server reported both halves.
    ///
    /// Near 100% means the prefix held and only the newly-appended tokens were
    /// prefilled; near 0% means the server re-evaluated the whole prompt, which is
    /// exactly what an append-only prompt exists to prevent.
    pub fn cache_hit_percent(&self) -> Option<u32> {
        let cached = self.cached_prompt_tokens?;
        let prefilled = self.prefilled_prompt_tokens?;
        let total = cached + prefilled;
        (total > 0).then(|| ((cached as f64 / total as f64) * 100.0).round() as u32)
    }

    /// Did the server stop because it hit the token cap?
    ///
    /// False when the reason is unknown — see [`Self::finish_reason`].
    pub fn was_truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

/// The health of an inference backend, as seen by a lightweight probe.
///
/// The crucial distinction is [`NoModel`](BackendHealth::NoModel) vs
/// [`Ready`](BackendHealth::Ready): an OpenAI-compatible router/shim can answer `/models`
/// (advertising a model from static config) while **no weights are actually loaded** — a real
/// completion is the only thing that proves the model is serving. So a `/models` ping alone
/// would report a hollow shim as healthy; the probe must attempt an actual (tiny) generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendHealth {
    /// A real (tiny) completion succeeded — the model is loaded and serving.
    Ready,
    /// The endpoint is reachable (HTTP responds) but a real completion failed — typically the
    /// router is up but no model is loaded (e.g. 0 VRAM), or the model name is wrong.
    NoModel { detail: String },
    /// The endpoint could not be reached at all (connection refused, DNS, connect timeout).
    Unreachable { detail: String },
}

impl BackendHealth {
    /// True only when a real completion succeeded — the sole state safe to start a run in.
    pub fn is_ready(&self) -> bool {
        matches!(self, BackendHealth::Ready)
    }

    /// Classify from the two probe outcomes: whether the endpoint was *reachable* at all
    /// (any HTTP response, even an error), and the result of the tiny completion. Pure, so
    /// the state machine is unit-testable without a server.
    ///
    /// * completion ok → [`Ready`](BackendHealth::Ready).
    /// * completion failed but endpoint reachable → [`NoModel`](BackendHealth::NoModel)
    ///   (the shim answers but nothing serves).
    /// * completion failed and endpoint unreachable → [`Unreachable`](BackendHealth::Unreachable).
    pub fn classify(reachable: bool, completion: std::result::Result<(), String>) -> Self {
        match completion {
            Ok(()) => BackendHealth::Ready,
            Err(detail) if reachable => BackendHealth::NoModel { detail },
            Err(detail) => BackendHealth::Unreachable { detail },
        }
    }
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
            Some(content) => Ok(GenerateResponse::new(content)),
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
    fn health_classify_completion_ok_is_ready() {
        // A successful completion is Ready regardless of the /models reachability signal.
        assert_eq!(BackendHealth::classify(true, Ok(())), BackendHealth::Ready);
        assert_eq!(BackendHealth::classify(false, Ok(())), BackendHealth::Ready);
        assert!(BackendHealth::Ready.is_ready());
    }

    #[test]
    fn health_classify_reachable_but_no_completion_is_no_model() {
        // The regression: a shim answers /models (reachable) but the completion fails because
        // no weights are loaded. Must be NoModel, NOT Ready — a /models-only check would lie.
        let h = BackendHealth::classify(true, Err("HTTP 503: no model loaded".into()));
        assert_eq!(
            h,
            BackendHealth::NoModel {
                detail: "HTTP 503: no model loaded".into()
            }
        );
        assert!(!h.is_ready());
    }

    #[test]
    fn health_classify_unreachable_when_endpoint_dead() {
        let h = BackendHealth::classify(false, Err("request failed: connection refused".into()));
        assert_eq!(
            h,
            BackendHealth::Unreachable {
                detail: "request failed: connection refused".into()
            }
        );
        assert!(!h.is_ready());
    }

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
            Ok(GenerateResponse::new(last.to_uppercase()))
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
        let backend = CallbackBackend::new("ok", seam_caps(), |_r| Ok(GenerateResponse::new("ok")));
        let dynamic: &dyn ModelBackend = &backend;
        let req = GenerateRequest::new(vec![Message::user("x")]);
        assert_eq!(dynamic.generate(&req).unwrap().content, "ok");
    }
}
