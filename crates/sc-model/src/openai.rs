//! OpenAI-compatible HTTP backend (spec 02 — the *primary path*).
//!
//! One adapter covers every server that speaks the OpenAI `/v1/chat/completions`
//! shape: **Ollama's compat endpoint, llama.cpp's `--api`, vLLM, LM Studio**, and
//! hosted OpenAI-compatible servers. That breadth is why spec 02 makes this the
//! first adapter we ship.
//!
//! The [`ModelBackend`] trait is synchronous, so this uses a blocking HTTP client
//! (`ureq`) — no async runtime, in keeping with the rest of the gateway.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use sc_proto::{DcError, Result};
use serde::Deserialize;

use crate::{
    BackendHealth, Capabilities, GenerateRequest, GenerateResponse, ModelBackend, OutputConstraint,
    Role, ToolCallRecord, ToolCalling,
};

/// A backend that talks to any OpenAI-compatible chat-completions endpoint.
///
/// Construct it with the server's base URL (e.g. `http://localhost:11434/v1` for
/// Ollama, `http://localhost:8080/v1` for llama.cpp) and the model name. An
/// optional bearer token covers hosted servers; local ones ignore it.
pub struct OpenAiBackend {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    caps: Capabilities,
    agent: ureq::Agent,
    /// Optional cooperative cancel flag: when set true mid-stream, `generate_streaming`
    /// stops reading the SSE and drops the connection (aborting the request). `None` =
    /// not cancellable (the default).
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Send llama.cpp's `cache_prompt: true` so a shared prefix (the system prompt, the
    /// conversation so far) is reused across calls instead of re-evaluated. Other local
    /// servers ignore the unknown field, but **Gemini's OpenAI-compat endpoint rejects
    /// unknown names with HTTP 400**, so a hosted backend turns it off via
    /// [`OpenAiBackend::with_prompt_cache`].
    prompt_cache: bool,
    /// Ask llama.cpp's `/tokenize` for exact counts (see
    /// [`ModelBackend::count_tokens`]). Off for the hosted providers that have no
    /// such endpoint; [`OpenAiBackend::with_tokenizer`] overrides either way.
    tokenizer: bool,
    /// `/tokenize` answers by content hash, so a stable prompt prefix (the system
    /// preamble, a pinned file) costs one round-trip per run, not one per turn.
    tokenize_cache: Mutex<HashMap<u64, usize>>,
    /// Set the first time `/tokenize` fails or answers non-2xx. After that every
    /// count is `None` without a request: a server without the endpoint costs one
    /// refused call, not one per segment per turn.
    tokenizer_dead: AtomicBool,
    /// A short-timeout client for `/tokenize`. The main agent's five-minute timeout
    /// suits a slow generation, not a count -- a server that has stopped answering
    /// must not stall the prompt builder for five minutes per segment.
    tokenize_agent: ureq::Agent,
}

/// Memoised `/tokenize` answers kept before the memo is cleared. Clear-when-full
/// is deliberately simpler than eviction: a whole run fits well inside this, and
/// the cost of a miss is one fast request.
const TOKENIZE_CACHE_CAPACITY: usize = 4096;

/// Does this base URL belong to a hosted provider that answers an unknown JSON field with
/// HTTP 400? Gemini's OpenAI-compat endpoint does (`Invalid JSON payload received. Unknown
/// name "cache_prompt"`), and OpenAI's own API is strict too. A local llama.cpp, Ollama or
/// vLLM ignores what it does not know, so everything else defaults to sending the flag.
fn rejects_unknown_fields(base_url: &str) -> bool {
    let url = base_url.to_ascii_lowercase();
    url.contains("generativelanguage.googleapis.com") || url.contains("api.openai.com")
}

impl OpenAiBackend {
    /// Build a backend pointing at `base_url` (with or without a trailing slash)
    /// using `model`. The advertised context window defaults to a conservative
    /// small-model budget; override it with [`OpenAiBackend::with_context_tokens`].
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let model = model.into();
        Self {
            // A descriptive, stable id for logs/reports (spec 03).
            name: "openai-compat".to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key: None,
            caps: Capabilities {
                // Conservative default for a small local model; the real window
                // is server/model-specific and capped by config (spec 02/05).
                max_context_tokens: 8_192,
                // OpenAI-compat servers vary in tool-calling support. We default
                // to plain completion (prompt+parse+repair, the safe floor) and
                // let callers opt into native FC with `with_native_tools` once
                // they know the served model supports it (spec 02).
                tool_calling: ToolCalling::None,
                on_device: false,
            },
            // Don't let ureq turn a non-2xx into a transport error — we read the
            // body ourselves to surface the server's error detail (spec 02).
            //
            // A generous global timeout: a reasoning model generating a long structured
            // reply (e.g. a full work-decomposition JSON array) on a big prompt can take
            // a couple of minutes. ureq's default timeout cut these off, so `generate()`
            // returned a transport error → the workflow's retries all timed out → an
            // empty artifact → "decomposition produced no content" (observed live
            // 2026-06-14: the restaurant-site decomposition that works fine with a long
            // HTTP timeout). 5 minutes covers the slowest local model without hanging
            // forever on a truly dead backend.
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(std::time::Duration::from_secs(300)))
                .build()
                .into(),
            cancel: None,
            // Off by default for the hosted providers known to reject unknown fields;
            // on for everything else (llama.cpp is the one that uses it).
            prompt_cache: !rejects_unknown_fields(&base_url),
            // The same providers have no `/tokenize`; everything else gets one probe.
            tokenizer: !rejects_unknown_fields(&base_url),
            tokenize_cache: Mutex::new(HashMap::new()),
            tokenizer_dead: AtomicBool::new(false),
            tokenize_agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(std::time::Duration::from_secs(15)))
                .build()
                .into(),
        }
    }

    /// Whether to ask the server's `/tokenize` endpoint for exact token counts
    /// (default: on, except for the hosted providers that have no such endpoint).
    /// With it off, [`ModelBackend::count_tokens`] answers `None` and the context
    /// manager falls back to its estimator.
    pub fn with_tokenizer(mut self, enabled: bool) -> Self {
        self.tokenizer = enabled;
        self
    }

    /// The server root: llama.cpp mounts its own endpoints (`/tokenize`, `/props`,
    /// `/health`) beside the OpenAI-compatible `/v1`, not under it.
    fn server_root(&self) -> &str {
        self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url)
    }

    /// POST `{root}/tokenize` with `text` and count the tokens it returns. `None` on
    /// any failure -- transport, non-2xx, or a body without a `tokens` array.
    fn fetch_token_count(&self, text: &str) -> Option<usize> {
        let url = format!("{}/tokenize", self.server_root());
        let mut call = self
            .tokenize_agent
            .post(&url)
            .header("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            call = call.header("Authorization", &format!("Bearer {key}"));
        }
        let mut resp = call
            .send_json(serde_json::json!({ "content": text }))
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.body_mut().read_to_string().ok()?;
        parse_token_count(&body)
    }

    /// Whether to send llama.cpp's `cache_prompt: true` on every request (default: on).
    /// Turn it off for a provider that rejects unknown fields — Gemini's OpenAI-compat
    /// endpoint answers `Invalid JSON payload received. Unknown name ...` with a 400.
    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Attach a cooperative cancel flag. When another thread sets it true, an in-flight
    /// [`generate_streaming`] stops at the next SSE line and returns what it has so far.
    pub fn with_cancel(mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Attach a bearer token (for hosted OpenAI-compatible servers).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the advertised context budget (e.g. from `doctor`/config).
    pub fn with_context_tokens(mut self, tokens: usize) -> Self {
        self.caps.max_context_tokens = tokens;
        self
    }

    /// Best-effort: query the server's `/models` and adopt the real context window it
    /// serves the model at (llama.cpp returns `data[0].meta.n_ctx`). The hardcoded 8192
    /// default badly under-budgets a model actually served at e.g. 24576 — the prompt is
    /// squeezed to a third of the window, forcing file-by-file navigation and stalls.
    ///
    /// On ANY failure (endpoint absent, server doesn't expose `n_ctx`, parse error) the
    /// existing `max_context_tokens` is kept — this never fails construction, so a server
    /// that doesn't advertise the window simply keeps the conservative default.
    pub fn with_detected_context(mut self) -> Self {
        if let Some(n) = self.fetch_n_ctx() {
            self.caps.max_context_tokens = n;
        }
        self
    }

    /// Probe backend health with a **real** tiny completion (not just a `/models` ping),
    /// because a router/shim can advertise a model via `/models` while no weights are loaded —
    /// only a completion proves the model is serving (see [`BackendHealth`]).
    ///
    /// Uses a short, dedicated timeout (`timeout_secs`) so a dead backend fails fast instead of
    /// hanging on this backend's generous generation timeout. Distinguishes "reachable but no
    /// model" from "unreachable" by whether the `/models` endpoint gave *any* HTTP response.
    pub fn health_probe(&self, timeout_secs: u64) -> BackendHealth {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
            .build()
            .into();

        // Reachability: did the endpoint give ANY HTTP response? (Even a 4xx/5xx counts —
        // it means something is listening and speaking HTTP, i.e. a router/shim is up.)
        let models_url = format!("{}/models", self.base_url);
        let mut get = agent.get(&models_url);
        if let Some(key) = &self.api_key {
            get = get.header("Authorization", &format!("Bearer {key}"));
        }
        let reachable = get.call().is_ok();

        // The real test: a 1-token completion. Success ⇒ a model is actually loaded.
        let completion = self.probe_completion(&agent);
        BackendHealth::classify(reachable, completion)
    }

    /// Fire a minimal `chat/completions` (`max_tokens: 1`) against `agent` and reduce it to
    /// `Ok(())` on a 2xx with a choice, or `Err(detail)` describing the failure. The cheapest
    /// request that still exercises the model path.
    fn probe_completion(&self, agent: &ureq::Agent) -> std::result::Result<(), String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": "."}],
            "max_tokens": 1,
            "stream": false,
        });
        let mut call = agent.post(&url).header("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            call = call.header("Authorization", &format!("Bearer {key}"));
        }
        let mut resp = match call.send_json(&body) {
            Ok(r) => r,
            Err(e) => return Err(format!("request failed: {e}")),
        };
        let status = resp.status();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if !status.is_success() {
            // The server responded but rejected the request — surface a trimmed reason (e.g.
            // "model not found", "no model loaded"). Reachable-but-not-ready → NoModel.
            let snippet = text.chars().take(200).collect::<String>();
            return Err(format!("HTTP {}: {}", status.as_u16(), snippet.trim()));
        }
        // A 2xx with a parseable choice is proof the model produced a token.
        if serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("choices").and_then(|c| c.get(0)).cloned())
            .is_some()
        {
            Ok(())
        } else {
            Err("2xx but no completion choice in response".to_string())
        }
    }

    /// GET `{base_url}/models` and pull `data[0].meta.n_ctx`. `None` on any error.
    fn fetch_n_ctx(&self) -> Option<usize> {
        let url = format!("{}/models", self.base_url);
        let mut call = self.agent.get(&url);
        if let Some(key) = &self.api_key {
            call = call.header("Authorization", &format!("Bearer {key}"));
        }
        let mut resp = call.call().ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.body_mut().read_to_string().ok()?;
        parse_n_ctx(&body)
    }

    /// Declare that the served model supports OpenAI-style function calling, so
    /// the strategy layer may attach a [`OutputConstraint::Tools`] constraint and
    /// this backend will forward it as `tools`/`tool_choice` (spec 02).
    pub fn with_native_tools(mut self) -> Self {
        self.caps.tool_calling = ToolCalling::OpenAiStyle;
        self
    }

    /// Declare GBNF grammar-constrained decoding (llama.cpp's `grammar` field).
    /// Prefer [`OpenAiBackend::llama_cpp`] for the common case.
    pub fn with_grammar(mut self) -> Self {
        self.caps.tool_calling = ToolCalling::Gbnf;
        self
    }

    /// A llama.cpp server backend: OpenAI-compatible HTTP **plus** GBNF
    /// grammar-constrained decoding — the strongest tool-call guarantee on tiny
    /// models (spec 02). llama.cpp's `--api` accepts a `grammar` field on the
    /// chat-completions request, which this backend forwards from a
    /// [`OutputConstraint::Grammar`].
    pub fn llama_cpp(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let mut b = Self::new(base_url, model).with_grammar();
        b.name = "llama-cpp".to_string();
        b
    }

    /// The chat-completions endpoint URL.
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// The one request body both paths send — `stream` is the only difference.
    ///
    /// The streaming path used to build its own body without the constraint, so a
    /// streamed run silently lost its `tools`/`grammar` while the transcript still
    /// recorded the constraint as sent. One builder means the two cannot drift again.
    fn build_body(&self, req: &GenerateRequest, stream: bool) -> serde_json::Value {
        let messages = wire_messages(&req.messages);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
            "stream": stream,
        });

        // llama.cpp's prompt cache: reuse the evaluated shared prefix across calls.
        // Gated because Gemini's compat endpoint 400s on any unknown field.
        if self.prompt_cache {
            body["cache_prompt"] = serde_json::Value::Bool(true);
        }
        if let Some(seed) = req.seed {
            body["seed"] = serde_json::json!(seed);
        }
        if !req.stop.is_empty() {
            body["stop"] = serde_json::json!(req.stop);
        }

        // Apply the request's output constraint with whatever this server speaks.
        // Native FC → tools/tool_choice; GBNF → llama.cpp's `grammar` extension.
        // A constraint the backend can't honor is simply not attached — the
        // strategy layer only sends one it negotiated via capabilities (spec 02).
        match &req.constraint {
            Some(OutputConstraint::Tools(tools))
                if self.caps.tool_calling == ToolCalling::OpenAiStyle =>
            {
                let defs: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect();
                body["tools"] = serde_json::Value::Array(defs);
                body["tool_choice"] = serde_json::json!("required");
            }
            Some(OutputConstraint::Grammar(g)) if self.caps.tool_calling == ToolCalling::Gbnf => {
                // llama.cpp accepts a GBNF grammar via this non-standard field.
                body["grammar"] = serde_json::Value::String(g.clone());
            }
            _ => {}
        }

        body
    }

    /// Streaming completion: like [`ModelBackend::generate`], but sets `"stream": true` and
    /// invokes `on_token` with each content delta as the server emits it (SSE). Returns the
    /// full concatenated text at the end (so callers get the same result as `generate` plus a
    /// live view). This is what powers the "watch it type" UI.
    ///
    /// The real SSE implementation of [`ModelBackend::generate_streaming`] (kept as an
    /// inherent method too so existing callers with a concrete `OpenAiBackend` keep working).
    pub fn generate_streaming(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<GenerateResponse> {
        let started = std::time::Instant::now();
        let result = self.generate_streaming_inner(req, on_token);
        self.log_call("generate_streaming", req, started, &result);
        result
    }

    /// The real SSE body of [`Self::generate_streaming`], split out so the public method can
    /// time it and log the transcript around it.
    fn generate_streaming_inner(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<GenerateResponse> {
        use std::io::BufRead;

        let body = self.build_body(req, true);

        let mut call = self.agent.post(&self.endpoint());
        if let Some(key) = &self.api_key {
            call = call.header("Authorization", &format!("Bearer {key}"));
        }
        let mut resp = call.send_json(&body).map_err(|e| {
            DcError::Backend(format!("stream request to {} failed: {e}", self.endpoint()))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(DcError::Backend(format!(
                "{} returned HTTP {}: {}",
                self.endpoint(),
                status.as_u16(),
                detail.trim()
            )));
        }

        // Read the SSE stream line by line. Each event is `data: {json}`; `data: [DONE]`
        // ends it. We pull `choices[0].delta.content` from each chunk and stream it out.
        let reader = std::io::BufReader::new(resp.body_mut().as_reader());
        let mut full = String::new();
        let mut calls = StreamingCalls::default();
        let mut finish_reason: Option<String> = None;
        let mut prompt_tokens: Option<usize> = None;
        // The prefix-cache split arrives on the same final chunk as `usage`.
        let mut split = CacheSplit::default();
        for line in reader.lines() {
            // Cooperative cancel: if the caller flagged a stop, quit reading and drop the
            // reader/connection so the request aborts. Return the partial text gathered so far.
            if let Some(c) = &self.cancel {
                if c.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
            let line = match line {
                Ok(l) => l,
                // A read error mid-stream is NOT the model stopping. Record it as a
                // truncation so the caller can tell a dropped connection from a
                // reply that genuinely ended -- otherwise a half-written tool call
                // reads as malformed JSON from the model.
                Err(_) => {
                    finish_reason = Some("error".to_string());
                    break;
                }
            };
            let payload = match line.strip_prefix("data:") {
                Some(p) => p.trim(),
                None => continue, // blank line or non-data field
            };
            if payload == "[DONE]" {
                break;
            }
            // The stop reason arrives on its own chunk, usually the last one before
            // [DONE] and usually with an empty delta -- so it must be read separately
            // from the content, not inside the `if let Some(delta)` below.
            if let Some(r) = parse_stream_finish_reason(payload) {
                finish_reason = Some(r);
            }
            if let Some(n) = parse_stream_prompt_tokens(payload) {
                prompt_tokens = Some(n);
            }
            // Keep the last chunk that carried a split; earlier chunks have none.
            let chunk_split = parse_cache_split(payload);
            if chunk_split != CacheSplit::default() {
                split = chunk_split;
            }
            // A native call streams as `delta.tool_calls`, indexed, with the
            // arguments arriving in fragments. Accumulate it here or the streaming
            // path loses the call entirely — which it did, silently: `parse_stream_delta`
            // only ever read `content`/`reasoning_content`, so a native-FC turn over
            // SSE produced an empty reply.
            calls.absorb(payload);
            if let Some(delta) = parse_stream_delta(payload) {
                if !delta.is_empty() {
                    full.push_str(&delta);
                    on_token(&delta);
                }
            }
        }
        // Streaming now reports its stop reason like the non-streaming path.
        //
        // It used to hardcode `None`, which meant `was_truncated()` was ALWAYS false
        // while streaming -- so `HarnessFault::ReplyTruncated` could never fire there.
        // That mattered: `sc-iterate` (the desktop and web interactive path) sets
        // `stream = true` unconditionally, so the GUI was blind to the exact failure
        // the eval had been hardened against, where a reply cut off at the token cap
        // reads as a model declining to act.
        // A grammar-constrained reply arrives entirely inside `reasoning_content`; unwrap it
        // so the tool call survives instead of being stripped as thinking.
        let full = unwrap_reasoning_only(&full).unwrap_or(full);
        // Same precedence as the non-streaming path: a native call, normalised to the
        // uniform `{"tool":…}` text, wins over whatever text also arrived.
        let records = calls.finish();
        let content = match records.first() {
            Some(tc) => record_to_text(tc),
            None => full,
        };
        let mut out = GenerateResponse::with_finish_reason(content, finish_reason);
        out.tool_calls = records;
        out.prompt_tokens = prompt_tokens;
        out.cached_prompt_tokens = split.cached;
        out.prefilled_prompt_tokens = split.prefilled;
        out.prompt_ms = split.prompt_ms;
        Ok(out)
    }
}

/// Tool calls being assembled from an SSE stream.
///
/// OpenAI streams a call as a series of `delta.tool_calls` entries carrying an
/// `index`: the first usually has the id and name, and the `arguments` string arrives
/// in fragments across later chunks. Indexed rather than positional because a model
/// may interleave two calls.
#[derive(Debug, Default)]
struct StreamingCalls {
    /// `index -> (id, name, arguments-so-far)`, kept ordered by index at the end.
    parts: std::collections::BTreeMap<u64, ToolCallRecord>,
}

impl StreamingCalls {
    /// Fold one SSE chunk in. Unknown or call-free chunks are no-ops.
    fn absorb(&mut self, payload: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        let Some(items) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("tool_calls"))
            .and_then(|t| t.as_array())
        else {
            return;
        };
        for (pos, item) in items.iter().enumerate() {
            let idx = item
                .get("index")
                .and_then(|i| i.as_u64())
                .unwrap_or(pos as u64);
            let slot = self.parts.entry(idx).or_default();
            if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                if !id.is_empty() {
                    slot.id = id.to_string();
                }
            }
            let Some(f) = item.get("function") else {
                continue;
            };
            if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                if !name.is_empty() {
                    slot.name = name.to_string();
                }
            }
            if let Some(args) = f.get("arguments").and_then(|a| a.as_str()) {
                slot.arguments.push_str(args);
            }
        }
    }

    /// The assembled calls, in index order. A slot that never got a name is dropped:
    /// it is a fragment of a call the stream was cut off before naming, not a call.
    fn finish(self) -> Vec<ToolCallRecord> {
        self.parts
            .into_values()
            .filter(|c| !c.name.is_empty())
            .collect()
    }
}

/// Pull `choices[0].finish_reason` out of one SSE chunk, when it carries one.
///
/// Arrives on its own chunk near the end of the stream, normally with an empty
/// delta, so it is read separately from the content. `Some("length")` is the
/// truncation case the loop needs to see.
fn parse_stream_finish_reason(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    v.get("choices")?
        .get(0)?
        .get("finish_reason")?
        .as_str()
        .map(str::to_string)
}

/// Pull the content delta out of one SSE chunk's JSON: `choices[0].delta.content`. Returns
/// `None` for a chunk with no content (e.g. the role-announcing first chunk, or a finish
/// chunk). Also handles reasoning models that stream into `reasoning_content`.
fn parse_stream_delta(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let delta = v.get("choices")?.get(0)?.get("delta")?;
    if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
        return Some(c.to_string());
    }
    // A reasoning model streams its thinking in a SEPARATE field. Emitting it raw made it
    // indistinguishable from the answer, so the chat panel rendered a page of "Wait — I'm
    // Tiel-Coder... Actually, let me reconsider" as if it were the reply.
    //
    // Wrap it in `<think>` tags — the shape every consumer here already strips (`strip_think`,
    // `visible_so_far`). That keeps ONE representation of "this is reasoning" rather than
    // teaching each caller about a second one, and models that inline their own `<think>`
    // tags in `content` already produce exactly this.
    if let Some(r) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
        if r.is_empty() {
            return None;
        }
        return Some(format!("<think>{r}</think>"));
    }
    None
}

/// Does this streamed reply consist ONLY of reasoning, with no ordinary content?
///
/// A reasoning model puts its thinking in `reasoning_content`, which is wrapped in `<think>`
/// above so consumers can strip it. But when decoding is grammar-constrained the model's
/// ENTIRE output is the tool call, and this server still delivers it in `reasoning_content` --
/// verified directly: a GBNF request returned `content: ""` and
/// `reasoning_content: {"tool": "search_code", "query": "def draw_trails"}`, a perfect call in
/// 18 tokens. Wrapped and then stripped, that call was destroyed and the harness reported
/// "no JSON tool object found in your reply".
///
/// So: if nothing but reasoning arrived, the reasoning IS the reply. The non-streaming path
/// has always had this fallback; streaming did not.
fn unwrap_reasoning_only(full: &str) -> Option<String> {
    let t = full.trim();
    if !t.starts_with("<think>") || !t.ends_with("</think>") {
        return None;
    }
    let inner = t.strip_prefix("<think>")?.strip_suffix("</think>")?.trim();
    // Only when it is a tool call. Ordinary prose reasoning must stay hidden -- unwrapping
    // that would put "Wait, let me re-read..." in front of the user as the answer.
    (inner.starts_with('{') && inner.ends_with('}')).then(|| inner.to_string())
}

// ---- wire types (a minimal slice of the OpenAI schema) ----
//
// The request is built as a `serde_json::Value` rather than a struct so the
// optional `tools` / `tool_choice` (native FC) and llama.cpp's `grammar`
// extension can be attached only when a constraint asks for them.

#[derive(Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    /// The server's own token accounting, when it reports it. `prompt_tokens` is
    /// the number the harness's counter is checked against.
    #[serde(default)]
    usage: Option<WireUsage>,
    /// llama.cpp's per-response timings, which carry the prefix-cache split
    /// (`cache_n` / `prompt_n`) the OpenAI schema has no field for. Absent on
    /// every other server.
    #[serde(default)]
    timings: Option<WireTimings>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<usize>,
    /// OpenAI's (and llama.cpp's) breakdown of the prompt count. The fallback
    /// source for the cached half when `timings` is absent.
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptDetails>,
}

#[derive(Deserialize)]
struct WirePromptDetails {
    #[serde(default)]
    cached_tokens: Option<usize>,
}

/// llama.cpp's `timings` block. The authoritative split: `cache_n` tokens came
/// from the KV cache, `prompt_n` tokens were actually prefilled, and `prompt_ms`
/// is what that prefill cost in wall clock.
#[derive(Deserialize)]
struct WireTimings {
    #[serde(default)]
    cache_n: Option<usize>,
    #[serde(default)]
    prompt_n: Option<usize>,
    #[serde(default)]
    prompt_ms: Option<f64>,
}

/// The prefix-cache split for one response: `(cached, prefilled, prompt_ms)`.
///
/// Every field is independently optional because the two sources disagree about
/// what they carry: llama.cpp's `timings` has all three, OpenAI's
/// `prompt_tokens_details` has only the cached count (so the prefilled half is
/// derived), and a server with neither yields all `None` rather than a
/// fabricated zero -- "the server did not say" and "nothing was cached" are
/// different facts and must not print the same.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct CacheSplit {
    cached: Option<usize>,
    prefilled: Option<usize>,
    prompt_ms: Option<f64>,
}

/// Reduce the two reporting shapes to one [`CacheSplit`].
///
/// `timings` wins when present: it is measured by the server for this request,
/// whereas the OpenAI-shaped details only ever carry the cached side. When only
/// the details are there the prefilled half is `prompt_tokens - cached`, which is
/// exact as long as the server counts them over the same prompt (llama.cpp does;
/// its own `cache_n + prompt_n == prompt_tokens`).
fn cache_split(
    timings: Option<&WireTimings>,
    details: Option<&WirePromptDetails>,
    prompt_tokens: Option<usize>,
) -> CacheSplit {
    if let Some(t) = timings {
        if t.cache_n.is_some() || t.prompt_n.is_some() {
            return CacheSplit {
                cached: t.cache_n,
                prefilled: t.prompt_n,
                prompt_ms: t.prompt_ms,
            };
        }
    }
    let cached = details.and_then(|d| d.cached_tokens);
    CacheSplit {
        cached,
        // Only derivable when the server said how big the prompt was.
        prefilled: match (prompt_tokens, cached) {
            (Some(total), Some(c)) => Some(total.saturating_sub(c)),
            _ => None,
        },
        prompt_ms: timings.and_then(|t| t.prompt_ms),
    }
}

/// Pull the cache split out of one JSON object (a whole response body, or one SSE
/// chunk -- llama.cpp puts `timings` and `usage` on the final chunk of a stream in
/// the same shape it uses for a non-streamed body).
fn parse_cache_split(payload: &str) -> CacheSplit {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return CacheSplit::default();
    };
    let timings: Option<WireTimings> = v
        .get("timings")
        .and_then(|t| serde_json::from_value(t.clone()).ok());
    let details: Option<WirePromptDetails> = v
        .get("usage")
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| serde_json::from_value(d.clone()).ok());
    let prompt_tokens = v
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|n| n.as_u64())
        .map(|n| n as usize);
    cache_split(timings.as_ref(), details.as_ref(), prompt_tokens)
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireResponseMessage,
    /// "stop", "tool_calls", or "length" when the reply was cut off at `max_tokens`.
    /// Kept because a truncated reply is indistinguishable from a short one by its
    /// content alone -- see `GenerateResponse::finish_reason`.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireResponseMessage {
    /// Plain-completion / grammar path puts the text here. Optional because the
    /// native-FC path may return only `tool_calls`.
    #[serde(default)]
    content: Option<String>,
    /// Thinking models (e.g. Gemma 4, Qwen3) put their internal reasoning here;
    /// if the reply was truncated mid-think, `content` is empty but the answer is
    /// forming here, so we fall back to it rather than returning nothing.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// Native function-calling path: the structured call(s) the model chose.
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Deserialize)]
struct WireToolCall {
    /// The server's id for this call, when it supplies one (llama.cpp often does
    /// not). Kept so the result can be paired back to it verbatim.
    #[serde(default)]
    id: Option<String>,
    function: WireFunction,
}

impl WireToolCall {
    /// The structured record kept on the assistant [`crate::Message`] so the next
    /// request replays this call in the shape it arrived in.
    fn to_record(&self) -> ToolCallRecord {
        ToolCallRecord::new(
            self.id.clone().unwrap_or_default(),
            self.function.name.clone(),
            self.function.arguments.clone(),
        )
    }
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    /// JSON-encoded argument object (a *string* per the OpenAI schema).
    #[serde(default)]
    arguments: String,
}

/// Pull `data[0].meta.n_ctx` out of a `/models` response body. Host-testable (takes the
/// raw JSON string) so the detection logic is verified without a live server. `None` if the
/// body doesn't parse or doesn't carry a positive `n_ctx`.
fn parse_n_ctx(body: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let n = v
        .get("data")?
        .as_array()?
        .first()?
        .get("meta")?
        .get("n_ctx")?
        .as_u64()?;
    (n > 0).then_some(n as usize)
}

/// The token count in a `/tokenize` reply: `{"tokens":[...]}`. `None` if the body
/// does not parse or has no `tokens` array (any other shape is not a count).
fn parse_token_count(body: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get("tokens")?.as_array()?.len())
}

/// Pull `usage.prompt_tokens` out of one SSE chunk, when it carries one. llama.cpp
/// puts `usage` on the final chunk; most chunks have none.
fn parse_stream_prompt_tokens(payload: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    v.get("usage")?
        .get("prompt_tokens")?
        .as_u64()
        .map(|n| n as usize)
}

/// The memo key for a piece of text.
fn content_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Render the whole conversation, keeping call/result pairing intact.
///
/// [`wire_message`] renders one message and cannot see the sequence, but the two halves
/// of a native call are two messages: the assistant turn carrying `tool_calls`, and the
/// `role:"tool"` result naming one of those ids. When the validity guard drops an
/// assistant turn's calls (its `arguments` were truncated mid-string and are not JSON),
/// the result that followed it is left naming an id no longer in the request — dangling,
/// malformed, and rejected by strict servers. So a `tool` message whose id no assistant
/// turn actually shipped is demoted to a plain user message, which is precisely the
/// pre-native shape and what the harness sent for its entire life before that round trip.
fn wire_messages(msgs: &[crate::Message]) -> Vec<serde_json::Value> {
    let shipped: std::collections::HashSet<String> = msgs
        .iter()
        .filter(|m| replays_calls(m))
        .flat_map(|m| m.tool_calls.iter().map(|tc| tc.wire_id()))
        .collect();
    msgs.iter()
        .map(|m| match (&m.role, &m.tool_call_id) {
            (Role::Tool, Some(id)) if !shipped.contains(id) => {
                serde_json::json!({"role": role_str(Role::User), "content": m.content})
            }
            _ => wire_message(m),
        })
        .collect()
}

/// Whether this message's native calls go out as a structured `tool_calls` array.
///
/// **The validity guard.** A call whose `arguments` is not parseable JSON must never
/// reach the wire: the server parses that string to render its chat template, and
/// llama.cpp answers a malformed one with HTTP 500 — killing the whole task. The
/// truncation salvage (`repair_truncated_file_write`) recovers usable work from exactly
/// such a reply, so the harness legitimately holds calls it must not replay natively.
/// [`crate::Message::assistant_with_calls`] already drops them at construction; this
/// re-checks because `Message`'s fields are public and other crates populate them
/// directly. It is all-or-nothing per turn, matching the constructor, so the observation
/// pairing above stays decidable.
fn replays_calls(m: &crate::Message) -> bool {
    !m.tool_calls.is_empty() && m.tool_calls.iter().all(ToolCallRecord::is_replayable)
}

/// Render ONE message in the OpenAI wire shape.
///
/// The whole point of this function is that an assistant turn which made a native
/// tool call goes back out AS a native tool call. The server's chat template is what
/// writes that turn's markup (`<tool_call>…</tool_call><|im_end|>` for the ChatML
/// family), and it only does so for a structured `tool_calls` array — a flattened
/// `{"tool":…}` string in `content` renders as ordinary assistant prose, teaching the
/// model a house style its own template never emits and that has no stop token in it.
///
/// Everything without a native call renders exactly as it always did: a bare
/// `{"role":…,"content":…}` pair, byte-identical, so the `ParseRepair`/`Grammar`
/// paths and every existing recording are untouched.
fn wire_message(m: &crate::Message) -> serde_json::Value {
    if replays_calls(m) {
        let calls: Vec<serde_json::Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.wire_id(),
                    "type": "function",
                    "function": { "name": tc.name, "arguments": tc.arguments },
                })
            })
            .collect();
        // `content` stays alongside the calls. A template that renders both shows the
        // model its own reasoning as well as the call; one that renders only the calls
        // (the ChatML family) ignores it. Sending null instead would DISCARD a
        // reasoning model's visible thinking from its own history.
        return serde_json::json!({
            "role": role_str(m.role),
            "content": m.content,
            "tool_calls": calls,
        });
    }
    match &m.tool_call_id {
        Some(id) => {
            serde_json::json!({"role": role_str(m.role), "content": m.content, "tool_call_id": id})
        }
        None => serde_json::json!({"role": role_str(m.role), "content": m.content}),
    }
}

/// Normalize a native `tool_calls[0]` back into the harness's uniform tool-call
/// string: `{"tool":"<name>", ...args}`. This lets the same `ParseRepair`
/// extractor validate every strategy's output — native FC included.
fn tool_call_to_text(tc: &WireToolCall) -> String {
    normalized_call_text(&tc.function.name, &tc.function.arguments)
}

/// The same normalisation for a call assembled off the stream.
fn record_to_text(tc: &ToolCallRecord) -> String {
    normalized_call_text(&tc.name, &tc.arguments)
}

/// `name` + JSON-encoded `arguments` -> the harness's uniform `{"tool":…}` string.
fn normalized_call_text(name: &str, arguments: &str) -> String {
    let args: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
    let mut obj = serde_json::Map::new();
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    if let Some(map) = args.as_object() {
        for (k, v) in map {
            obj.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(obj).to_string()
}

impl ModelBackend for OpenAiBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn generate_streaming(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<GenerateResponse> {
        // Route the trait method to the real SSE implementation (the inherent method).
        OpenAiBackend::generate_streaming(self, req, on_token)
    }

    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        let started = std::time::Instant::now();
        let result = self.generate_inner(req);
        self.log_call("generate", req, started, &result);
        result
    }

    /// Exact count from llama.cpp's `/tokenize`, memoised by content.
    ///
    /// The estimator this replaces undercounted the same server by 23% (see
    /// `sc_context::estimate_tokens`), and every margin stacked to cover it -- the
    /// effective-window fraction, the reply reserve -- was context the model never
    /// got to use. Asking the server means the budget is measured against the
    /// tokenizer that will actually run.
    ///
    /// Probed once: the first failure (no such endpoint, a transport error, a body
    /// without `tokens`) marks the tokenizer dead and every later call answers
    /// `None` at once, so the context manager settles on its estimator after a
    /// single refused request rather than one per segment.
    fn count_tokens(&self, text: &str) -> Option<usize> {
        if !self.tokenizer || self.tokenizer_dead.load(Ordering::Relaxed) {
            return None;
        }
        if text.is_empty() {
            return Some(0);
        }
        let key = content_hash(text);
        if let Some(n) = self.lock_tokenize_cache().get(&key) {
            return Some(*n);
        }
        let Some(n) = self.fetch_token_count(text) else {
            self.tokenizer_dead.store(true, Ordering::Relaxed);
            return None;
        };
        let mut cache = self.lock_tokenize_cache();
        if cache.len() >= TOKENIZE_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(key, n);
        Some(n)
    }
}

impl OpenAiBackend {
    fn lock_tokenize_cache(&self) -> std::sync::MutexGuard<'_, HashMap<u64, usize>> {
        // A poisoned memo is still a valid memo: a panic mid-insert leaves at worst
        // a missing entry, never a wrong one.
        self.tokenize_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

impl OpenAiBackend {
    /// Emit one [`crate::transcript`] entry for a completed call — the full messages sent and
    /// the reply/error received. Always-on (best-effort); the transcript module no-ops when
    /// logging is disabled, so this is cheap on the hot path.
    fn log_call(
        &self,
        call: &str,
        req: &GenerateRequest,
        started: std::time::Instant,
        result: &Result<GenerateResponse>,
    ) {
        if !crate::transcript::is_enabled() {
            return;
        }
        let messages: Vec<(&str, &str)> = req
            .messages
            .iter()
            .map(|m| (role_str(m.role), m.content.as_str()))
            .collect();
        let constraint = req.constraint.as_ref().map(|c| match c {
            OutputConstraint::Tools(_) => ("tools", ""),
            OutputConstraint::Grammar(g) => ("grammar", g.as_str()),
        });
        let endpoint = self.endpoint();
        // Own the error string so its borrow lives across the log call.
        let err_text = result.as_ref().err().map(|e| e.to_string());
        let result_ref: std::result::Result<&str, &str> = match result {
            Ok(r) => Ok(r.content.as_str()),
            Err(_) => Err(err_text.as_deref().unwrap_or("<error>")),
        };
        crate::transcript::log(crate::transcript::Entry {
            call,
            model: &self.model,
            endpoint: &endpoint,
            messages: &messages,
            constraint,
            temperature: req.temperature,
            max_tokens: req.max_tokens as u32,
            result: result_ref,
            ms: started.elapsed().as_millis(),
        });
    }

    /// The real request/response body of [`ModelBackend::generate`], split out so the public
    /// method can time it and log the transcript around it.
    fn generate_inner(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        let body = self.build_body(req, false);

        let mut call = self.agent.post(&self.endpoint());
        if let Some(key) = &self.api_key {
            call = call.header("Authorization", &format!("Bearer {key}"));
        }

        let mut resp = call
            .send_json(&body)
            .map_err(|e| DcError::Backend(format!("request to {} failed: {e}", self.endpoint())))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(DcError::Backend(format!(
                "{} returned HTTP {}: {}",
                self.endpoint(),
                status.as_u16(),
                detail.trim()
            )));
        }

        let parsed: WireResponse = resp.body_mut().read_json().map_err(|e| {
            DcError::Backend(format!(
                "could not parse response from {}: {e}",
                self.endpoint()
            ))
        })?;

        let choice =
            parsed.choices.into_iter().next().ok_or_else(|| {
                DcError::Backend(format!("{} returned no choices", self.endpoint()))
            })?;
        let finish_reason = choice.finish_reason;
        let message = choice.message;
        let (prompt_tokens, split) = {
            let timings = parsed.timings;
            let usage = parsed.usage;
            let prompt_tokens = usage.as_ref().and_then(|u| u.prompt_tokens);
            let details = usage
                .as_ref()
                .and_then(|u| u.prompt_tokens_details.as_ref());
            let split = cache_split(timings.as_ref(), details, prompt_tokens);
            (prompt_tokens, split)
        };

        // Prefer a native tool call (normalized to the uniform string shape); else
        // plain text content; else the reasoning block (thinking models that ran
        // out of tokens mid-think leave content empty but reasoning populated).
        // The normalised `{"tool":…}` text stays exactly as it was — the whole
        // extraction/validation path reads it — but the STRUCTURED call is kept
        // beside it so the loop can put the turn back on the wire in the shape the
        // server handed it over. See `ToolCallRecord`.
        let records: Vec<ToolCallRecord> =
            message.tool_calls.iter().map(|t| t.to_record()).collect();
        let content = if let Some(tc) = message.tool_calls.first() {
            tool_call_to_text(tc)
        } else {
            let text = message.content.unwrap_or_default();
            if text.trim().is_empty() {
                message.reasoning_content.unwrap_or_default()
            } else {
                text
            }
        };

        let mut out = GenerateResponse::with_finish_reason(content, finish_reason);
        out.tool_calls = records;
        out.prompt_tokens = prompt_tokens;
        out.cached_prompt_tokens = split.cached;
        out.prefilled_prompt_tokens = split.prefilled;
        out.prompt_ms = split.prompt_ms;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{mpsc, Arc};
    use std::thread;

    /// Read a full HTTP/1.1 request off `sock`: headers up to the blank line,
    /// then the body by `Content-Length`. Returns the raw request text. Draining
    /// it fully avoids a connection reset when the client's write outruns the
    /// server's read.
    fn drain_http_request(sock: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = sock.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(idx) = text.find("\r\n\r\n") {
                let content_len = text[..idx]
                    .lines()
                    .find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if text.len() - (idx + 4) >= content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A throwaway one-shot HTTP/1.1 server: accepts a single connection, hands
    /// the raw request back over a channel, and replies with `response`. Enough to
    /// exercise the adapter end-to-end with no external deps or network.
    /// Like [`stub_server`] but with a caller-chosen content type, for serving an
    /// SSE stream rather than a JSON body.
    fn stub_server_raw(
        response: &'static str,
        content_type: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let raw = drain_http_request(&mut sock);
            let _ = tx.send(raw);
            let reply = format!(
                "HTTP/1.1 200 OK
Content-Type: {content_type}
Content-Length: {}
Connection: close

{}",
                response.len(),
                response
            );
            sock.write_all(reply.as_bytes()).unwrap();
            sock.flush().unwrap();
        });

        (format!("http://{addr}/v1"), rx)
    }

    fn stub_server(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let raw = drain_http_request(&mut sock);
            let _ = tx.send(raw);

            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            sock.write_all(reply.as_bytes()).unwrap();
            sock.flush().unwrap();
        });

        (format!("http://{addr}/v1"), rx)
    }

    /// Like [`stub_server`] but answers EVERY connection with the same reply, with a
    /// caller-chosen status line, and counts the requests it served. For the
    /// tokenizer tests, whose whole point is how many requests were made.
    fn counting_stub(
        status_line: &'static str,
        response: &'static str,
    ) -> (String, Arc<AtomicUsize>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();

        let counter = Arc::clone(&hits);
        thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                let raw = drain_http_request(&mut sock);
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(raw);
                let reply = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                let _ = sock.write_all(reply.as_bytes());
                let _ = sock.flush();
            }
        });

        (format!("http://{addr}/v1"), hits, rx)
    }

    /// **`/tokenize` is asked once per text, at the server root.**
    ///
    /// The prompt is rebuilt from mostly the same segments every turn, and each
    /// count is an HTTP round-trip; memoising by content is what makes exact
    /// counting affordable. And the endpoint lives beside `/v1`, not under it --
    /// `/v1/tokenize` is a 404 on llama.cpp.
    #[test]
    fn count_tokens_asks_tokenize_once_per_text() {
        let (base, hits, rx) = counting_stub("200 OK", r#"{"tokens":[1,2,3,4,5]}"#);
        let backend = OpenAiBackend::new(base, "m");

        assert_eq!(backend.count_tokens("hello there"), Some(5));
        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /tokenize "), "got: {raw}");
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains(r#""content":"hellothere""#), "got: {raw}");

        assert_eq!(backend.count_tokens("hello there"), Some(5));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the second count is a memo hit"
        );

        assert_eq!(backend.count_tokens("something else"), Some(5));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "new text is a new question");

        // Empty text needs no server to count.
        assert_eq!(backend.count_tokens(""), Some(0));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// **A server without `/tokenize` is asked exactly once.**
    ///
    /// Ollama, vLLM and LM Studio all speak `/v1/chat/completions` and none serve
    /// llama.cpp's `/tokenize`. One 404 settles it; after that every count is
    /// `None` with no request, so the context manager estimates instead of paying
    /// a failed round-trip per segment per turn.
    #[test]
    fn a_missing_tokenize_endpoint_is_probed_once() {
        let (base, hits, _rx) = counting_stub("404 Not Found", r#"{"error":"no such route"}"#);
        let backend = OpenAiBackend::new(base, "m");

        assert_eq!(backend.count_tokens("first"), None);
        assert_eq!(backend.count_tokens("second"), None);
        assert_eq!(backend.count_tokens("first"), None);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "probed once, then remembered"
        );
    }

    /// A 200 whose body is not a token list is just as dead as a 404.
    #[test]
    fn a_tokenize_reply_without_tokens_marks_the_tokenizer_dead() {
        let (base, hits, _rx) = counting_stub("200 OK", r#"{"message":"ok"}"#);
        let backend = OpenAiBackend::new(base, "m");
        assert_eq!(backend.count_tokens("x"), None);
        assert_eq!(backend.count_tokens("y"), None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// The hosted providers that reject unknown fields have no `/tokenize` either:
    /// they are off by default, and an explicit opt-out makes no request at all.
    #[test]
    fn strict_hosted_providers_and_opt_outs_never_probe() {
        for url in [
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            "https://api.openai.com/v1",
        ] {
            let b = OpenAiBackend::new(url, "m");
            assert!(!b.tokenizer, "{url} must not probe /tokenize");
            assert_eq!(b.count_tokens("x"), None);
        }

        let (base, hits, _rx) = counting_stub("200 OK", r#"{"tokens":[1]}"#);
        let off = OpenAiBackend::new(base.clone(), "m").with_tokenizer(false);
        assert_eq!(off.count_tokens("x"), None);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "opted out: no request");

        // And a local server is on by default, overridable either way.
        assert!(OpenAiBackend::new("http://localhost:11436/v1", "m").tokenizer);
        assert!(
            OpenAiBackend::new("https://api.openai.com/v1", "m")
                .with_tokenizer(true)
                .tokenizer
        );
    }

    #[test]
    fn server_root_strips_only_the_v1_suffix() {
        assert_eq!(
            OpenAiBackend::new("http://localhost:11436/v1/", "m").server_root(),
            "http://localhost:11436"
        );
        // A base without /v1 is its own root.
        assert_eq!(
            OpenAiBackend::new("http://localhost:8080", "m").server_root(),
            "http://localhost:8080"
        );
    }

    #[test]
    fn parses_a_tokenize_reply() {
        assert_eq!(parse_token_count(r#"{"tokens":[1,2,3]}"#), Some(3));
        assert_eq!(parse_token_count(r#"{"tokens":[]}"#), Some(0));
        // With pieces, still one entry per token.
        assert_eq!(
            parse_token_count(r#"{"tokens":[{"id":1,"piece":"a"},{"id":2,"piece":"b"}]}"#),
            Some(2)
        );
        assert_eq!(parse_token_count(r#"{"error":"x"}"#), None);
        assert_eq!(parse_token_count("not json"), None);
    }

    /// **The server's own prompt count reaches the caller**, on both paths -- it is
    /// the one number the harness's accounting can be checked against.
    #[test]
    fn parses_the_servers_prompt_token_count() {
        let (base, _rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],
                "usage":{"prompt_tokens":42,"completion_tokens":1,"total_tokens":43}}"#,
        );
        let resp = OpenAiBackend::new(base, "m")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(resp.prompt_tokens, Some(42));

        // Absent usage is unknown, not zero.
        let (base, _rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#);
        let resp = OpenAiBackend::new(base, "m")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(resp.prompt_tokens, None);

        // Streaming: llama.cpp puts usage on the final chunk.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\
                   \"usage\":{\"prompt_tokens\":17,\"completion_tokens\":1}}\n\n\
                   data: [DONE]\n\n";
        let (base, _rx) = stub_server_raw(sse, "text/event-stream");
        let resp = OpenAiBackend::new(base, "m")
            .generate_streaming(
                &GenerateRequest::new(vec![Message::user("hi")]),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(resp.prompt_tokens, Some(17));
        assert_eq!(resp.content, "ok");
    }

    /// **The prefix-cache split reaches the caller, on both paths.**
    ///
    /// `prompt_tokens` counts what we SEND, which an append-only prompt cannot
    /// move; `cache_n`/`prompt_n` count what the server had to RE-PREFILL, which
    /// is the only place the work shows up. Pinned with the exact body llama.cpp
    /// b10015 returns, verified live at localhost:11436.
    #[test]
    fn parses_the_prefix_cache_split_from_llama_cpp_timings() {
        const BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hi"},
                "finish_reason":"stop"}],
            "usage":{"prompt_tokens":191,"completion_tokens":5,"total_tokens":196,
                     "prompt_tokens_details":{"cached_tokens":179}},
            "timings":{"cache_n":179,"prompt_n":12,"prompt_ms":538.0,
                       "predicted_n":5,"predicted_ms":296.8}}"#;
        let (base, _rx) = stub_server(BODY);
        let resp = OpenAiBackend::new(base, "m")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(resp.prompt_tokens, Some(191));
        assert_eq!(resp.cached_prompt_tokens, Some(179));
        assert_eq!(resp.prefilled_prompt_tokens, Some(12));
        assert_eq!(resp.prompt_ms, Some(538.0));
        // 179 of 191 served from cache.
        assert_eq!(resp.cache_hit_percent(), Some(94));

        // Streaming: llama.cpp repeats `timings` on the final chunk.
        const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":191,\"prompt_tokens_details\":{\"cached_tokens\":179}},\
             \"timings\":{\"cache_n\":179,\"prompt_n\":12,\"prompt_ms\":538.0}}\n\n\
             data: [DONE]\n\n";
        let (base, _rx) = stub_server_raw(SSE, "text/event-stream");
        let resp = OpenAiBackend::new(base, "m")
            .generate_streaming(
                &GenerateRequest::new(vec![Message::user("hi")]),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(resp.content, "hi");
        assert_eq!(resp.cached_prompt_tokens, Some(179));
        assert_eq!(resp.prefilled_prompt_tokens, Some(12));
        assert_eq!(resp.prompt_ms, Some(538.0));
    }

    /// **A server that reports only the OpenAI-shaped detail still yields both halves.**
    ///
    /// `prompt_tokens_details.cached_tokens` is the cached side alone; the
    /// prefilled side is the remainder of the prompt, which is only derivable
    /// because the same `usage` says how big the prompt was.
    #[test]
    fn derives_the_prefilled_half_when_only_usage_details_are_reported() {
        let (base, _rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],
                "usage":{"prompt_tokens":1000,"prompt_tokens_details":{"cached_tokens":900}}}"#,
        );
        let resp = OpenAiBackend::new(base, "m")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(resp.cached_prompt_tokens, Some(900));
        assert_eq!(resp.prefilled_prompt_tokens, Some(100));
        assert_eq!(resp.prompt_ms, None, "no timings block, no prefill time");
        assert_eq!(resp.cache_hit_percent(), Some(90));
    }

    /// **A server that reports neither says nothing, not zero.**
    ///
    /// Every non-llama.cpp backend lands here, and a run against one must not
    /// print "0% cache hit" -- that is a claim the server never made.
    #[test]
    fn a_server_without_cache_reporting_yields_all_none() {
        const BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],
            "usage":{"prompt_tokens":42,"completion_tokens":1}}"#;
        let (base, _rx) = stub_server(BODY);
        let resp = OpenAiBackend::new(base, "m")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(resp.prompt_tokens, Some(42));
        assert_eq!(resp.cached_prompt_tokens, None);
        assert_eq!(resp.prefilled_prompt_tokens, None);
        assert_eq!(resp.prompt_ms, None);
        assert_eq!(resp.cache_hit_percent(), None);

        // And the same over a stream with no usage at all.
        const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                           data: [DONE]\n\n";
        let (base, _rx) = stub_server_raw(SSE, "text/event-stream");
        let resp = OpenAiBackend::new(base, "m")
            .generate_streaming(
                &GenerateRequest::new(vec![Message::user("hi")]),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(resp.cached_prompt_tokens, None);
        assert_eq!(resp.prefilled_prompt_tokens, None);
    }

    /// **Against the real server: the counter agrees with the tokenizer that ran.**
    ///
    /// Needs llama.cpp serving tiel-coder-35b at localhost:11436 (the ops repo's
    /// compose), so it is ignored by default:
    /// `cargo test -p sc-model live_tokenizer -- --ignored --nocapture`.
    ///
    /// The prompt is made large on purpose: `count_tokens` counts TEXT, and the
    /// chat template wraps each message in a few tokens of markup the server also
    /// counts. On a two-line chat that markup is a fifth of the total; on a couple
    /// of thousand tokens it is inside the 5% this asserts. A miss here means the
    /// template costs more than the per-message allowance the context manager adds,
    /// which is the number to revisit.
    #[test]
    #[ignore = "needs llama.cpp at localhost:11436"]
    fn live_tokenizer_matches_the_servers_prompt_count() {
        let backend = OpenAiBackend::new("http://localhost:11436/v1", "tiel-coder-35b");
        let code: String = (0..250)
            .map(|i| format!("fn compute_{i}(x: u32) -> u32 {{ x.wrapping_mul({i}) + 1 }}\n"))
            .collect();
        let messages = vec![
            Message::system("You are terse. Reply with one word."),
            Message::user(format!("Say ok. Here is some code for context:\n{code}")),
        ];
        let counted: usize = messages
            .iter()
            .map(|m| {
                backend
                    .count_tokens(&m.content)
                    .expect("the server serves /tokenize")
            })
            .sum();

        let mut req = GenerateRequest::new(messages);
        req.max_tokens = 8;
        let resp = backend.generate(&req).unwrap();
        let served = resp
            .prompt_tokens
            .expect("llama.cpp reports usage.prompt_tokens");

        let gap = (served as f64 - counted as f64).abs() / served as f64;
        eprintln!(
            "counted {counted} vs served {served}: gap {:.1}%",
            gap * 100.0
        );
        assert!(
            gap <= 0.05,
            "count_tokens {counted} is {:.1}% off the server's {served}",
            gap * 100.0
        );
    }

    #[test]
    fn sends_openai_shaped_request_and_parses_the_reply() {
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"hello back"}}]}"#);

        let backend = OpenAiBackend::new(base, "gemma4:e4b");
        let req = GenerateRequest::new(vec![Message::system("be terse"), Message::user("say hi")]);
        let resp = backend.generate(&req).unwrap();

        // The reply was parsed out of the OpenAI envelope.
        assert_eq!(resp.content, "hello back");

        // And the request we sent was OpenAI-shaped: right path, model, and roles.
        // Normalize whitespace so the assertions don't care whether the client
        // serialized compact or pretty JSON.
        let raw = rx.recv().unwrap();
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(raw.starts_with("POST /v1/chat/completions"), "got: {raw}");
        assert!(compact.contains("\"model\":\"gemma4:e4b\""), "got: {raw}");
        assert!(compact.contains("\"role\":\"system\""), "got: {raw}");
        assert!(compact.contains("\"role\":\"user\""), "got: {raw}");
        assert!(compact.contains("\"content\":\"sayhi\""), "got: {raw}");
    }

    /// A reply cut off at `max_tokens` must be distinguishable from a short one.
    ///
    /// Without `finish_reason` the two are identical on the wire once you look only
    /// at `content`, and the agent loop reports the truncation as the model declining
    /// to emit a tool call -- which cost 54 dead turns on one SWE-bench instance
    /// before anyone read the transcript. Pinned so the field cannot be quietly
    /// dropped again in a refactor of the choice unpacking.
    #[test]
    fn reports_when_the_server_truncated_the_reply() {
        let (base, _rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"the fix is to "},"finish_reason":"length"}]}"#,
        );

        let backend = OpenAiBackend::new(base, "gemma4:e4b");
        let resp = backend
            .generate(&GenerateRequest::new(vec![Message::user("explain")]))
            .unwrap();

        assert_eq!(resp.finish_reason.as_deref(), Some("length"));
        assert!(resp.was_truncated(), "a `length` stop is a truncation");
    }

    /// The ordinary case, and the reason `was_truncated` is not `finish_reason.is_some()`.
    #[test]
    fn a_normal_stop_is_not_a_truncation() {
        let (base, _rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
        );

        let resp = OpenAiBackend::new(base, "gemma4:e4b")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();

        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert!(!resp.was_truncated());
    }

    /// Servers that omit the field must still parse. `None` means *unknown*, and
    /// `was_truncated` reports false -- callers that need certainty check the
    /// `Option` itself.
    #[test]
    fn a_missing_finish_reason_parses_as_unknown() {
        let (base, _rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#);

        let resp = OpenAiBackend::new(base, "gemma4:e4b")
            .generate(&GenerateRequest::new(vec![Message::user("hi")]))
            .unwrap();

        assert_eq!(resp.finish_reason, None);
        assert!(!resp.was_truncated());
    }

    #[test]
    fn surfaces_http_errors_as_backend_errors() {
        // A server that replies 500 — the adapter must not pretend it succeeded.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            drain_http_request(&mut sock);
            let body = r#"{"error":"model not found"}"#;
            let reply = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(reply.as_bytes());
        });

        let backend = OpenAiBackend::new(format!("http://{addr}/v1"), "missing");
        let req = GenerateRequest::new(vec![Message::user("hi")]);
        let err = backend.generate(&req).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HTTP 500"), "got: {msg}");
        assert!(msg.contains("model not found"), "got: {msg}");
    }

    #[test]
    fn trims_trailing_slash_from_base_url() {
        let backend = OpenAiBackend::new("http://localhost:11434/v1/", "m");
        assert_eq!(
            backend.endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn advertises_a_small_model_context_budget_by_default() {
        let backend = OpenAiBackend::new("http://x/v1", "m");
        assert_eq!(backend.capabilities().max_context_tokens, 8_192);
        let bumped = OpenAiBackend::new("http://x/v1", "m").with_context_tokens(32_768);
        assert_eq!(bumped.capabilities().max_context_tokens, 32_768);
    }

    #[test]
    fn parses_n_ctx_from_a_models_payload() {
        // The real llama.cpp /models shape: data[0].meta.n_ctx is the served window.
        let body = r#"{"object":"list","data":[{"id":"qwen3-coder-30b","object":"model",
            "meta":{"n_vocab":151936,"n_ctx":24576,"n_embd":2048}}]}"#;
        assert_eq!(super::parse_n_ctx(body), Some(24_576));
    }

    #[test]
    fn parse_n_ctx_is_none_when_absent_or_malformed() {
        // A server that doesn't advertise n_ctx → None → caller keeps the 8192 default.
        assert_eq!(super::parse_n_ctx(r#"{"data":[{"id":"m"}]}"#), None);
        assert_eq!(super::parse_n_ctx(r#"{"data":[]}"#), None);
        assert_eq!(super::parse_n_ctx("not json"), None);
        // A zero/negative window is not usable.
        assert_eq!(
            super::parse_n_ctx(r#"{"data":[{"meta":{"n_ctx":0}}]}"#),
            None
        );
    }

    #[test]
    fn defaults_to_no_enforced_tool_calling() {
        assert_eq!(
            OpenAiBackend::new("http://x/v1", "m")
                .capabilities()
                .tool_calling,
            ToolCalling::None
        );
        assert_eq!(
            OpenAiBackend::new("http://x/v1", "m")
                .with_native_tools()
                .capabilities()
                .tool_calling,
            ToolCalling::OpenAiStyle
        );
    }

    #[test]
    fn forwards_native_tools_and_normalizes_the_tool_call_reply() {
        // Server returns a native function call (no `content`, a `tool_calls`).
        let (base, rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","tool_calls":[
                {"type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}
            ]}}]}"#,
        );

        let backend = OpenAiBackend::new(base, "gemma4:e4b").with_native_tools();
        let req = GenerateRequest::new(vec![Message::user("read a.txt")]).with_constraint(
            OutputConstraint::Tools(vec![crate::ToolSchema {
                name: "read_file".into(),
                description: "Read a file.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            }]),
        );
        let resp = backend.generate(&req).unwrap();

        // The native tool_call is normalized into the uniform `{"tool":...}` shape.
        let v: serde_json::Value = serde_json::from_str(&resp.content).unwrap();
        assert_eq!(v["tool"], "read_file");
        assert_eq!(v["path"], "a.txt");

        // And the request carried the OpenAI `tools` + `tool_choice` fields.
        let raw = rx.recv().unwrap();
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("\"tools\":["), "got: {raw}");
        assert!(compact.contains("\"name\":\"read_file\""), "got: {raw}");
        assert!(
            compact.contains("\"tool_choice\":\"required\""),
            "got: {raw}"
        );
    }

    #[test]
    fn llama_cpp_forwards_a_gbnf_grammar() {
        let (base, rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"{\"tool\":\"finish\"}"}}]}"#,
        );
        let backend = OpenAiBackend::llama_cpp(base, "gemma-e4b.gguf");
        assert_eq!(backend.capabilities().tool_calling, ToolCalling::Gbnf);
        assert_eq!(backend.name(), "llama-cpp");

        let req = GenerateRequest::new(vec![Message::user("go")])
            .with_constraint(OutputConstraint::Grammar("root ::= \"{}\"".into()));
        backend.generate(&req).unwrap();

        let raw = rx.recv().unwrap();
        assert!(raw.contains("grammar"), "grammar field missing: {raw}");
    }

    #[test]
    fn does_not_send_tools_when_backend_lacks_native_fc() {
        // Constraint present, but the backend defaults to ToolCalling::None — it
        // must NOT forward `tools` (the strategy layer wouldn't send one, but the
        // backend defends the contract too).
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        let backend = OpenAiBackend::new(base, "m"); // no with_native_tools()
        let req = GenerateRequest::new(vec![Message::user("hi")])
            .with_constraint(OutputConstraint::Tools(vec![]));
        backend.generate(&req).unwrap();

        let raw = rx.recv().unwrap();
        assert!(!raw.contains("tool_choice"), "must not force tools: {raw}");
    }

    /// A minimal SSE reply, for tests that only care about the request that was sent.
    const DONE_SSE: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";

    /// **The streaming path must carry the constraint too.**
    ///
    /// It built its own body without `tools`/`tool_choice`, so every streamed run
    /// (the desktop and web interactive path) lost its native-FC constraint while
    /// the transcript still recorded the constraint as sent. Pinned at the wire.
    #[test]
    fn a_streamed_request_carries_native_tools() {
        let (base, rx) = stub_server_raw(DONE_SSE, "text/event-stream");
        let backend = OpenAiBackend::new(base, "m").with_native_tools();
        let req = GenerateRequest::new(vec![Message::user("read a.txt")]).with_constraint(
            OutputConstraint::Tools(vec![crate::ToolSchema {
                name: "read_file".into(),
                description: "Read a file.".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
        );
        backend.generate_streaming(&req, &mut |_: &str| {}).unwrap();

        let raw = rx.recv().unwrap();
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("\"stream\":true"), "got: {raw}");
        assert!(compact.contains("\"tools\":["), "got: {raw}");
        assert!(compact.contains("\"name\":\"read_file\""), "got: {raw}");
        assert!(
            compact.contains("\"tool_choice\":\"required\""),
            "got: {raw}"
        );
    }

    /// The same drift for GBNF: a streamed llama.cpp run must send `grammar`.
    #[test]
    fn a_streamed_request_carries_the_grammar() {
        let (base, rx) = stub_server_raw(DONE_SSE, "text/event-stream");
        let backend = OpenAiBackend::llama_cpp(base, "m.gguf");
        let req = GenerateRequest::new(vec![Message::user("go")])
            .with_constraint(OutputConstraint::Grammar("root ::= \"{}\"".into()));
        backend.generate_streaming(&req, &mut |_: &str| {}).unwrap();

        let raw = rx.recv().unwrap();
        assert!(raw.contains("\"grammar\""), "grammar field missing: {raw}");
    }

    /// Both bodies ask llama.cpp to reuse the evaluated prefix; a provider that 400s
    /// on unknown fields (Gemini's compat endpoint) opts out via the builder.
    #[test]
    fn a_strict_hosted_provider_never_gets_the_cache_flag_by_default() {
        for url in [
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            "https://api.openai.com/v1",
        ] {
            let b = OpenAiBackend::new(url, "m");
            let body = b.build_body(&GenerateRequest::new(vec![]), false);
            assert!(
                body.get("cache_prompt").is_none(),
                "{url} must not get cache_prompt"
            );
        }
        let local = OpenAiBackend::new("http://localhost:11436/v1", "m");
        let body = local.build_body(&GenerateRequest::new(vec![]), false);
        assert_eq!(body["cache_prompt"], serde_json::json!(true));
    }

    #[test]
    fn every_body_asks_for_the_prompt_cache_unless_opted_out() {
        let req = GenerateRequest::new(vec![Message::user("hi")]);
        let backend = OpenAiBackend::new("http://x/v1", "m");
        for stream in [false, true] {
            let body = backend.build_body(&req, stream);
            assert_eq!(body["cache_prompt"], true, "stream={stream}: {body}");
            assert_eq!(body["stream"], stream);
        }

        let hosted = OpenAiBackend::new("http://x/v1", "m").with_prompt_cache(false);
        for stream in [false, true] {
            let body = hosted.build_body(&req, stream);
            assert!(
                body.get("cache_prompt").is_none(),
                "stream={stream}: must not send an unknown field to a strict provider: {body}"
            );
        }
    }

    /// `seed` and `stop` are sent only when the request sets them — an absent seed
    /// must not become `null` on the wire, and an empty stop list must not be sent.
    #[test]
    fn seed_and_stop_appear_only_when_set() {
        let backend = OpenAiBackend::new("http://x/v1", "m");

        let bare = backend.build_body(&GenerateRequest::new(vec![Message::user("hi")]), false);
        assert!(bare.get("seed").is_none(), "got: {bare}");
        assert!(bare.get("stop").is_none(), "got: {bare}");

        let pinned = GenerateRequest::new(vec![Message::user("hi")])
            .with_seed(42)
            .with_stop(["</answer>", "\n\n"]);
        for stream in [false, true] {
            let body = backend.build_body(&pinned, stream);
            assert_eq!(body["seed"], 42, "stream={stream}: {body}");
            assert_eq!(
                body["stop"],
                serde_json::json!(["</answer>", "\n\n"]),
                "stream={stream}: {body}"
            );
        }
    }

    /// The whole streaming path must carry the stop reason through, not just the
    /// parser. Pins the CALL SITE: a unit test on `parse_stream_finish_reason`
    /// still passes if the loop stops calling it.
    #[test]
    fn a_streamed_truncation_reaches_the_caller() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"the fix is \"}}]}\n\
                   \n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\
                   \n\
                   data: [DONE]\n\n";
        let (base, _rx) = stub_server_raw(sse, "text/event-stream");

        let backend = OpenAiBackend::new(base, "gemma4:e4b");
        let mut seen = String::new();
        let resp = backend
            .generate_streaming(
                &GenerateRequest::new(vec![Message::user("explain")]),
                &mut |d: &str| seen.push_str(d),
            )
            .unwrap();

        assert_eq!(resp.content, "the fix is ");
        assert_eq!(seen, "the fix is ", "deltas still reach the callback");
        assert!(
            resp.was_truncated(),
            "a streamed `length` stop must reach the caller, or ReplyTruncated \
             can never fire on the GUI path"
        );
    }

    /// **Streaming must report its stop reason, or truncation is invisible there.**
    ///
    /// The delta parser only ever read content, so `finish_reason` was hardcoded
    /// `None` on the streaming path and `was_truncated()` was always false --
    /// `HarnessFault::ReplyTruncated` could not fire. `sc-iterate` (the desktop and
    /// web interactive path) sets `stream = true` unconditionally, so the GUI was
    /// blind to the exact failure the eval had been hardened against.
    #[test]
    fn a_finish_chunk_yields_its_stop_reason() {
        // The truncation case: the reply was cut off at the token cap.
        assert_eq!(
            parse_stream_finish_reason(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#)
                .as_deref(),
            Some("length")
        );
        // The ordinary case.
        assert_eq!(
            parse_stream_finish_reason(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
                .as_deref(),
            Some("stop")
        );
        // A content chunk carries no stop reason -- it must not be mistaken for one.
        assert_eq!(
            parse_stream_finish_reason(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#),
            None
        );
        // An explicit null (llama.cpp sends these on every content chunk) is not a
        // reason either.
        assert_eq!(
            parse_stream_finish_reason(
                r#"{"choices":[{"delta":{"content":"x"},"finish_reason":null}]}"#
            ),
            None
        );
        // Garbage doesn't panic.
        assert_eq!(parse_stream_finish_reason("not json"), None);
    }

    #[test]
    fn parses_content_and_reasoning_deltas_and_ignores_control_chunks() {
        // A normal content delta.
        assert_eq!(
            parse_stream_delta(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#).as_deref(),
            Some("Hel")
        );
        // A reasoning-model delta streams into reasoning_content, and comes back TAGGED so
        // callers can tell thinking from answer. Returning it bare (as this once asserted)
        // is what let a model's private deliberation render as the chat reply.
        assert_eq!(
            parse_stream_delta(r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#)
                .as_deref(),
            Some("<think>think</think>")
        );
        // The role-announcing first chunk (no content) yields nothing.
        assert_eq!(
            parse_stream_delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
            None
        );
        // A finish chunk (empty delta) yields nothing.
        assert_eq!(
            parse_stream_delta(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            None
        );
        // Garbage doesn't panic.
        assert_eq!(parse_stream_delta("not json"), None);
    }
    // ---- the tool-call round trip (the Mellum2 corruption) -------------------
    //
    // Every test below exists because of one measured failure. Mellum2-12B's chat
    // template wraps calls in `<tool_call>...</tool_call>` and ends the turn with
    // `<|im_end|>`. The harness used to flatten each native call to a bare
    // `{"tool":...}` string and replay it as plain assistant content, so from turn 2
    // onward the model read its OWN history in a format its template never emits. It
    // imitated the history and never reached its stop token. Isolated, same server,
    // same six tools: 24 completion tokens and a clean stop on a faithful history,
    // 3,072 (the cap) with `{"tool":"finish"}</tool_call>` repeated ~60 times on the
    // flattened one. It cost 3 of the first 6 ladder rungs and 350-400k tokens.

    /// Parse the JSON body out of a raw HTTP request the stub server captured.
    fn body_of(raw: &str) -> serde_json::Value {
        let idx = raw.find("\r\n\r\n").expect("headers end");
        serde_json::from_str(&raw[idx + 4..]).expect("a JSON body")
    }

    /// **THE REGRESSION.** An assistant turn that made a native call goes out as a
    /// structured `tool_calls` array -- not as bare JSON stuffed in `content`.
    ///
    /// The `tool_calls` array is what makes the server's chat template write the
    /// model's own wrapper markup. Without it the template renders ordinary assistant
    /// prose, and the token that ends the model's turn is never in the history at all.
    #[test]
    fn an_assistant_turn_with_a_native_call_goes_out_as_structured_tool_calls() {
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        let call = ToolCallRecord::new("call_7", "read_file", r#"{"path":"lib.rs"}"#);
        let convo = vec![
            Message::system("be terse"),
            Message::user("fix the bug"),
            Message::assistant_with_calls(r#"{"tool":"read_file","path":"lib.rs"}"#, vec![call]),
            Message::tool("call_7", "read_file lib.rs:\nfn main() {}"),
        ];
        OpenAiBackend::new(base, "mellum")
            .generate(&GenerateRequest::new(convo))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);

        let a = &msgs[2];
        assert_eq!(a["role"], "assistant");
        let calls = a["tool_calls"].as_array().expect("structured tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_7");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"path":"lib.rs"}"#);

        // And the result is paired to that call, as a `tool` message.
        let t = &msgs[3];
        assert_eq!(t["role"], "tool");
        assert_eq!(t["tool_call_id"], "call_7");
        assert!(t["content"].as_str().unwrap().contains("fn main"));

        // The plain messages are untouched: no stray tool fields anywhere.
        assert!(msgs[0].get("tool_calls").is_none());
        assert!(msgs[1].get("tool_calls").is_none());
        assert!(msgs[1].get("tool_call_id").is_none());
    }

    /// A `write_file` whose arguments were CUT OFF at the token cap: exactly the bytes
    /// llama.cpp choked on. Valid enough for the harness's truncation salvage to recover
    /// work from, not valid JSON.
    fn truncated_args() -> String {
        // An opened `content` string that never closes — the reply ended mid-body.
        r#"{"path":"src/astar.rs","content":"    None
}

/// A* over a caller-supplied bool grid"#
            .to_string()
    }

    /// **THE REGRESSION (guard).** A tool call whose `arguments` is not valid JSON must
    /// NOT go on the wire as a structured call.
    ///
    /// The model's reply was truncated at the token cap mid-`write_file`. The harness's
    /// salvage still recovers usable work from it, but replaying those exact bytes inside
    /// a `tool_calls` array makes the server fail to parse its own request: llama.cpp
    /// answers `HTTP 500: Failed to parse tool call arguments as JSON ... missing closing
    /// quote` and the whole task dies at 0 steps (`SOLVER-ERR`). Seen on
    /// `engine-diagonal-path`. The fallback is the pre-native shape, which survived this
    /// for the harness's entire life: plain content, no `tool_calls`, and a plain USER
    /// observation with no dangling `tool_call_id`.
    #[test]
    fn a_truncated_tool_call_falls_back_to_plain_content_instead_of_a_500() {
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        let bad = ToolCallRecord::new("call_9", "write_file", truncated_args());
        assert!(!bad.is_replayable(), "these arguments really are malformed");

        let convo = vec![
            Message::user("write the pathfinder"),
            Message::assistant_with_calls(
                r#"{"tool":"write_file","path":"src/astar.rs"}"#,
                vec![bad],
            ),
            Message::tool(
                "call_9",
                "write_file src/astar.rs: wrote 812 bytes (truncated)",
            ),
        ];
        OpenAiBackend::new(base, "tiel")
            .generate(&GenerateRequest::new(convo))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);

        // The assistant turn went out as PLAIN CONTENT: no `tool_calls` field at all.
        let a = &msgs[1];
        assert_eq!(a["role"], "assistant");
        assert!(
            a.get("tool_calls").is_none(),
            "malformed arguments must never reach the wire: {a}"
        );
        assert_eq!(
            a["content"],
            r#"{"tool":"write_file","path":"src/astar.rs"}"#
        );

        // And its observation went out as a PLAIN USER message — pairing it to a call
        // that is no longer in the request would be malformed in its own right.
        let t = &msgs[2];
        assert_eq!(t["role"], "user", "no dangling tool result");
        assert!(t.get("tool_call_id").is_none(), "no dangling id: {t}");
        assert!(t["content"].as_str().unwrap().contains("812 bytes"));

        // Nothing anywhere in the body carries the unparseable bytes as an `arguments`.
        assert!(
            !body.to_string().contains("\\\"arguments\\\""),
            "no arguments field survives on this request"
        );
    }

    /// **Size is not the trigger — validity is.** A very large but WELL-FORMED argument
    /// object still round-trips natively. The 500 was about parseability; inventing a
    /// size cap would silently drop legitimate big writes.
    #[test]
    fn a_huge_but_valid_argument_object_still_goes_out_natively() {
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        let big =
            serde_json::json!({"path": "src/big.rs", "content": "x".repeat(200_000)}).to_string();
        let call = ToolCallRecord::new("call_big", "write_file", big.clone());
        assert!(call.is_replayable());

        OpenAiBackend::new(base, "tiel")
            .generate(&GenerateRequest::new(vec![
                Message::assistant_with_calls("{}", vec![call]),
                Message::tool("call_big", "wrote"),
            ]))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let msgs = body["messages"].as_array().unwrap();
        let calls = msgs[0]["tool_calls"].as_array().expect("still native");
        assert_eq!(calls[0]["function"]["arguments"], big);
        assert_eq!(msgs[1]["role"], "tool");
    }

    /// **A mixed conversation stays well-formed.** One good call, one truncated call,
    /// then an ordinary text turn: the good one goes out natively with its paired result,
    /// the bad one as plain content with a plain user observation, and every `tool`
    /// message left in the body names a call that is actually present.
    #[test]
    fn a_mixed_conversation_keeps_the_good_call_and_degrades_only_the_bad_one() {
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        let good = ToolCallRecord::new("call_a", "read_file", r#"{"path":"lib.rs"}"#);
        let bad = ToolCallRecord::new("call_b", "write_file", truncated_args());

        let convo = vec![
            Message::system("be terse"),
            Message::user("build it"),
            Message::assistant_with_calls(r#"{"tool":"read_file","path":"lib.rs"}"#, vec![good]),
            Message::tool(
                "call_a",
                "read_file lib.rs:
fn main() {}",
            ),
            Message::assistant_with_calls(
                r#"{"tool":"write_file","path":"src/astar.rs"}"#,
                vec![bad],
            ),
            Message::tool("call_b", "write_file: wrote 812 bytes (truncated)"),
            Message::assistant("I will append the rest."),
            Message::user("go on"),
        ];
        OpenAiBackend::new(base, "tiel")
            .generate(&GenerateRequest::new(convo))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 8);

        // The GOOD call is untouched: native, paired.
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap()[0]["id"], "call_a");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_a");

        // The BAD one degraded, both halves together.
        assert!(msgs[4].get("tool_calls").is_none());
        assert_eq!(msgs[5]["role"], "user");
        assert!(msgs[5].get("tool_call_id").is_none());

        // The ordinary turns are ordinary.
        assert_eq!(msgs[6]["role"], "assistant");
        assert!(msgs[6].get("tool_calls").is_none());
        assert_eq!(msgs[7]["role"], "user");

        // WELL-FORMEDNESS: every `tool_call_id` in the body names an id that some
        // assistant turn actually shipped. No dangling references anywhere.
        let shipped: std::collections::HashSet<String> = msgs
            .iter()
            .filter_map(|m| m.get("tool_calls").and_then(|c| c.as_array()))
            .flatten()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        for m in msgs {
            if let Some(id) = m.get("tool_call_id").and_then(|v| v.as_str()) {
                assert!(shipped.contains(id), "dangling tool_call_id {id}");
            }
        }
    }

    /// **The full round trip: parse a native reply, store it, send it back the same.**
    ///
    /// This is the property the fix is actually about -- what came off the wire on turn
    /// N is what goes back onto it on turn N+1.
    #[test]
    fn a_native_call_round_trips_back_out_in_the_shape_it_arrived_in() {
        // Turn 1: the server answers with a native tool call.
        const REPLY: &str = r#"{"choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"call_abc","type":"function",
              "function":{"name":"search_code","arguments":"{\"query\":\"draw_trails\"}"}}]},
            "finish_reason":"tool_calls"}]}"#;
        let (base, _rx) = stub_server(REPLY);
        let resp = OpenAiBackend::new(base, "mellum")
            .generate(&GenerateRequest::new(vec![Message::user("find it")]))
            .unwrap();

        // The normalised text the harness's extractor reads is UNCHANGED by this work.
        assert_eq!(
            resp.content,
            r#"{"query":"draw_trails","tool":"search_code"}"#
        );
        // And the structured call rode along beside it.
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_abc");
        assert_eq!(resp.tool_calls[0].name, "search_code");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"query":"draw_trails"}"#);

        // Turn 2: store that turn and rebuild the request.
        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        let convo = vec![
            Message::user("find it"),
            Message::assistant_with_calls(resp.content.clone(), resp.tool_calls.clone()),
            Message::tool(resp.tool_calls[0].wire_id(), "3 matches"),
        ];
        OpenAiBackend::new(base, "mellum")
            .generate(&GenerateRequest::new(convo))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let sent = &body["messages"][1]["tool_calls"][0];
        assert_eq!(sent["id"], "call_abc", "the server's own id survives");
        assert_eq!(sent["function"]["name"], "search_code");
        // Byte-for-byte the arguments string the server sent, not a re-serialisation.
        assert_eq!(sent["function"]["arguments"], r#"{"query":"draw_trails"}"#);
        assert_eq!(body["messages"][2]["tool_call_id"], "call_abc");
    }

    /// **The parse-repair path is unchanged.** A text-only reply carries no calls, and
    /// replaying it produces exactly the two-field `{"role","content"}` object it always
    /// did -- nothing added, so Tiel and every recorded run behave identically.
    #[test]
    fn a_text_reply_stores_and_replays_as_plain_content() {
        let (base, _rx) = stub_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"{\"tool\":\"finish\"}"}}]}"#,
        );
        let resp = OpenAiBackend::new(base, "tiel")
            .generate(&GenerateRequest::new(vec![Message::user("go")]))
            .unwrap();
        assert_eq!(resp.content, r#"{"tool":"finish"}"#);
        assert!(
            resp.tool_calls.is_empty(),
            "no native call, nothing to keep"
        );

        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        OpenAiBackend::new(base, "tiel")
            .generate(&GenerateRequest::new(vec![
                Message::user("go"),
                Message::assistant(resp.content.clone()),
                Message::user("observation"),
            ]))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let a = &body["messages"][1];
        assert_eq!(a["role"], "assistant");
        assert_eq!(a["content"], r#"{"tool":"finish"}"#);
        assert_eq!(
            a.as_object().unwrap().len(),
            2,
            "exactly role+content, byte-identical to before this change: {a}"
        );
        assert_eq!(body["messages"][2]["role"], "user");
    }

    /// A server that supplies no call id still produces a MATCHING pair -- the harness
    /// and the wire builder derive the same stand-in from the call itself. llama.cpp
    /// usually omits the id, so this is the common case, not the corner.
    #[test]
    fn a_call_without_a_server_id_still_pairs_to_its_result() {
        const REPLY: &str = r#"{"choices":[{"message":{"role":"assistant",
            "tool_calls":[{"type":"function",
              "function":{"name":"finish","arguments":"{\"summary\":\"done\"}"}}]}}]}"#;
        let (base, _rx) = stub_server(REPLY);
        let resp = OpenAiBackend::new(base, "mellum")
            .generate(&GenerateRequest::new(vec![Message::user("go")]))
            .unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert!(resp.tool_calls[0].id.is_empty(), "the server gave no id");

        let (base, rx) =
            stub_server(r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#);
        OpenAiBackend::new(base, "mellum")
            .generate(&GenerateRequest::new(vec![
                Message::assistant_with_calls(resp.content.clone(), resp.tool_calls.clone()),
                Message::tool(resp.tool_calls[0].wire_id(), "result"),
            ]))
            .unwrap();

        let body = body_of(&rx.recv().unwrap());
        let id = body["messages"][0]["tool_calls"][0]["id"].as_str().unwrap();
        assert!(!id.is_empty(), "a stand-in id was synthesised");
        assert_eq!(
            body["messages"][1]["tool_call_id"], id,
            "both halves derive the SAME id or the template cannot pair them"
        );
    }

    /// A streamed native call is assembled from its fragments and kept.
    ///
    /// The streaming path used to read only `content`/`reasoning_content`, so a
    /// native-FC turn over SSE returned an empty reply and no call at all.
    #[test]
    fn a_streamed_native_call_is_assembled_and_retained() {
        const SSE: &str = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\
             \"tool_calls\":[{\"index\":0,\"id\":\"call_s\",\"type\":\"function\",\
             \"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"\\\"lib.rs\\\"}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let (base, _rx) = stub_server_raw(SSE, "text/event-stream");
        let resp = OpenAiBackend::new(base, "mellum")
            .generate_streaming(
                &GenerateRequest::new(vec![Message::user("read it")]),
                &mut |_| {},
            )
            .unwrap();

        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_s");
        assert_eq!(resp.tool_calls[0].name, "read_file");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"path":"lib.rs"}"#);
        // Normalised to the same uniform text the non-streaming path produces.
        assert_eq!(resp.content, r#"{"path":"lib.rs","tool":"read_file"}"#);
    }
}

#[cfg(test)]
mod reasoning_stream {
    use super::*;

    /// **Reasoning deltas must be marked as reasoning.**
    ///
    /// They used to be returned bare, indistinguishable from answer text, so the chat
    /// panel rendered a reasoning model's private deliberation as the reply. Tagging
    /// them reuses the `<think>` shape every consumer already strips.
    #[test]
    fn a_reasoning_delta_is_tagged_and_content_is_not() {
        let r =
            parse_stream_delta(r#"{"choices":[{"delta":{"reasoning_content":"let me think"}}]}"#);
        assert_eq!(r.as_deref(), Some("<think>let me think</think>"));

        // Ordinary content passes through untouched — tagging it would hide the answer.
        let c = parse_stream_delta(r#"{"choices":[{"delta":{"content":"the answer"}}]}"#);
        assert_eq!(c.as_deref(), Some("the answer"));

        // An empty reasoning delta yields nothing, not an empty pair of tags.
        let e = parse_stream_delta(r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#);
        assert_eq!(e, None);
    }
}

#[cfg(test)]
mod reasoning_only_replies {
    use super::unwrap_reasoning_only;

    /// **A grammar-constrained call arrives as reasoning, and must survive.**
    ///
    /// Verified against the live server: a GBNF request returned `content: ""` and
    /// `reasoning_content: {"tool": "search_code", "query": "def draw_trails"}` — a perfect
    /// tool call in 18 tokens. The streaming path wrapped it in `<think>`, the chat panel
    /// stripped it, and the harness reported "no JSON tool object found in your reply".
    #[test]
    fn a_tool_call_delivered_as_reasoning_is_unwrapped() {
        assert_eq!(
            unwrap_reasoning_only(r#"<think>{"tool": "search_code", "query": "x"}</think>"#)
                .as_deref(),
            Some(r#"{"tool": "search_code", "query": "x"}"#)
        );
    }

    /// **Ordinary reasoning must stay hidden.**
    ///
    /// Unwrapping prose would put "Wait, let me re-read the code..." in front of the user as
    /// though it were the answer — the exact wall this `<think>` wrapping exists to prevent.
    #[test]
    fn prose_reasoning_stays_wrapped() {
        assert_eq!(
            unwrap_reasoning_only("<think>Wait, let me re-read the code.</think>"),
            None
        );
        // A reply that already has real content is untouched.
        assert_eq!(
            unwrap_reasoning_only(r#"<think>thinking</think>{"tool":"finish"}"#),
            None
        );
    }
}
