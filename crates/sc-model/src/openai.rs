//! OpenAI-compatible HTTP backend (spec 02 — the *primary path*).
//!
//! One adapter covers every server that speaks the OpenAI `/v1/chat/completions`
//! shape: **Ollama's compat endpoint, llama.cpp's `--api`, vLLM, LM Studio**, and
//! hosted OpenAI-compatible servers. That breadth is why spec 02 makes this the
//! first adapter we ship.
//!
//! The [`ModelBackend`] trait is synchronous, so this uses a blocking HTTP client
//! (`ureq`) — no async runtime, in keeping with the rest of the gateway.

use sc_proto::{DcError, Result};
use serde::Deserialize;

use crate::{
    Capabilities, GenerateRequest, GenerateResponse, ModelBackend, OutputConstraint, Role,
    ToolCalling,
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
        }
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
        use std::io::BufRead;

        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serde_json::json!({"role": role_str(m.role), "content": m.content}))
            .collect();
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
            "stream": true,
        });

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
                Err(_) => break,
            };
            let payload = match line.strip_prefix("data:") {
                Some(p) => p.trim(),
                None => continue, // blank line or non-data field
            };
            if payload == "[DONE]" {
                break;
            }
            if let Some(delta) = parse_stream_delta(payload) {
                if !delta.is_empty() {
                    full.push_str(&delta);
                    on_token(&delta);
                }
            }
        }
        Ok(GenerateResponse { content: full })
    }
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
    if let Some(r) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
        return Some(r.to_string());
    }
    None
}

// ---- wire types (a minimal slice of the OpenAI schema) ----
//
// The request is built as a `serde_json::Value` rather than a struct so the
// optional `tools` / `tool_choice` (native FC) and llama.cpp's `grammar`
// extension can be attached only when a constraint asks for them.

#[derive(Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireResponseMessage,
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
    function: WireFunction,
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

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Normalize a native `tool_calls[0]` back into the harness's uniform tool-call
/// string: `{"tool":"<name>", ...args}`. This lets the same `ParseRepair`
/// extractor validate every strategy's output — native FC included.
fn tool_call_to_text(tc: &WireToolCall) -> String {
    let args: serde_json::Value =
        serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
    let mut obj = serde_json::Map::new();
    obj.insert(
        "tool".to_string(),
        serde_json::Value::String(tc.function.name.clone()),
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
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serde_json::json!({"role": role_str(m.role), "content": m.content}))
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
            "stream": false,
        });

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

        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| DcError::Backend(format!("{} returned no choices", self.endpoint())))?;

        // Prefer a native tool call (normalized to the uniform string shape); else
        // plain text content; else the reasoning block (thinking models that ran
        // out of tokens mid-think leave content empty but reasoning populated).
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

        Ok(GenerateResponse { content })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
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

    #[test]
    fn parses_content_and_reasoning_deltas_and_ignores_control_chunks() {
        // A normal content delta.
        assert_eq!(
            parse_stream_delta(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#).as_deref(),
            Some("Hel")
        );
        // A reasoning-model delta streams into reasoning_content.
        assert_eq!(
            parse_stream_delta(r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#)
                .as_deref(),
            Some("think")
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
}
