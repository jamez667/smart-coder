//! The local web dashboard server (spec 01/06).
//!
//! Runs the agent on a worker thread feeding a [`Hub`]; serves the embedded
//! dashboard page and an incremental `/events?from=N` JSON feed the browser polls.
//! A blocking [`tiny_http`] server keeps us off an async runtime, consistent with
//! the rest of the codebase.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use sc_core::{run_agent_observed, AgentConfig, AgentReport, Confirmation, ToolCallStrategy};
use sc_model::ModelBackend;
use sc_tools::ToolRegistry;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::hub::{Hub, HubSink};
use crate::remote_confirm::RemoteConfirmer;

pub(crate) const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Everything needed to drive a run behind the dashboard. Backends are owned
/// (moved to the worker thread).
pub struct WebRun<B, A>
where
    B: ModelBackend + Send + 'static,
    A: ModelBackend + Send + 'static,
{
    pub backend: B,
    pub advisor: Option<A>,
    pub registry: ToolRegistry,
    pub strategy: Box<dyn ToolCallStrategy + Send + Sync>,
    pub instruction: String,
    pub workspace: PathBuf,
    pub config: AgentConfig,
}

/// Parse the `from=N` query of an `/events` request (defaults to 0).
pub fn parse_from(url: &str) -> usize {
    url.split_once("from=")
        .and_then(|(_, rest)| rest.split('&').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Build the JSON body for `/events?from=N` from the hub.
pub fn events_body(hub: &Hub, from: usize) -> String {
    let (events, next, done) = hub.since(from);
    let events_json = serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string());
    format!("{{\"events\":{events_json},\"next\":{next},\"done\":{done}}}")
}

/// Start the dashboard on `addr` (e.g. `127.0.0.1:0` for an OS-assigned port),
/// run the task, and serve until the run ends and the browser disconnects (or the
/// process is killed). Returns the bound URL via `on_ready` before blocking.
pub fn serve<B, A>(
    mut spec: WebRun<B, A>,
    addr: &str,
    token: &str,
    on_ready: impl FnOnce(String),
) -> std::io::Result<Option<AgentReport>>
where
    B: ModelBackend + Send + 'static,
    A: ModelBackend + Send + 'static,
{
    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    let url = format!("http://{}", server.server_addr());
    on_ready(url);

    let hub = Hub::new();

    // Inbound control seams (spec: remote drive). A RemoteConfirmer lets a connected
    // client approve/deny a confirm-gated command; the cancel flag lets it stop the
    // run. Both are wired into the config BEFORE the worker spawns, and clones are
    // kept here for the request router to drive.
    let confirmer = RemoteConfirmer::new(hub.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    spec.config.confirmer = Some(Arc::new(confirmer.clone()));
    spec.config.cancel = Some(cancel.clone());

    // Drive the agent on a worker thread; its sink feeds the hub.
    let agent_hub = hub.clone();
    let worker = thread::spawn(move || {
        let sink = HubSink::new(agent_hub);
        run_agent_observed(
            &spec.backend,
            spec.advisor.as_ref().map(|a| a as &dyn ModelBackend),
            &spec.registry,
            spec.strategy.as_ref(),
            &spec.instruction,
            &spec.workspace,
            &spec.config,
            &sink,
        )
    });

    // Serve requests until the run is done AND the client has drained the stream.
    // We keep serving after `done` so the final frame reaches the browser; a short
    // idle timeout then lets the process exit on its own if nobody's watching.
    for mut request in server.incoming_requests() {
        let response = route(&hub, &confirmer, &cancel, token, &mut request);
        let url = request.url().to_string();
        let _ = request.respond(response);
        // Stop once the run finished and the browser has read the terminal event.
        if hub.is_done() && url.starts_with("/events") && parse_from(&url) >= hub.len() {
            break;
        }
    }

    Ok(worker.join().ok().and_then(|r| r.ok()))
}

/// Route one request. GET routes carry the token as `?k=`; state-changing POST
/// routes require an `Authorization: Bearer <token>` header (the CSRF defense —
/// a cross-origin page can't set that header without a CORS preflight we never grant).
fn route(
    hub: &Hub,
    confirmer: &RemoteConfirmer,
    cancel: &Arc<AtomicBool>,
    token: &str,
    request: &mut Request,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let method = request.method().clone();
    let url = request.url().to_string();
    // The path without its query string, so `/` matches `/?k=...` (the URL the phone
    // opens always carries the token query, so an `== "/"` check would 404 it).
    let path = url.split('?').next().unwrap_or("/");
    match (&method, url.as_str()) {
        // Read routes: token in the query string (the phone opens a plain URL/QR).
        (Method::Get, _) if path == "/" || path.starts_with("/index") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            html(DASHBOARD_HTML)
        }
        (Method::Get, u) if u.starts_with("/events") => {
            if !query_token_ok(u, token) {
                return unauthorized();
            }
            json(&events_body(hub, parse_from(u)))
        }
        // Control routes: bearer header required, body is JSON.
        (Method::Post, u) if u.starts_with("/approve") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            resolve_from_body(request, confirmer, true)
        }
        (Method::Post, u) if u.starts_with("/deny") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            resolve_from_body(request, confirmer, false)
        }
        (Method::Post, u) if u.starts_with("/cancel") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            cancel.store(true, Ordering::Relaxed);
            confirmer.deny_all("run cancelled");
            json("{\"ok\":true}")
        }
        _ => Response::from_string("not found").with_status_code(404),
    }
}

/// The `k=<token>` query param matches, constant-time.
pub fn query_token_ok(url: &str, token: &str) -> bool {
    let got = url
        .split_once("k=")
        .map(|(_, rest)| rest.split('&').next().unwrap_or(""))
        .unwrap_or("");
    ct_eq(got.as_bytes(), token.as_bytes())
}

/// The `Authorization: Bearer <token>` header matches, constant-time.
pub(crate) fn bearer_ok(request: &Request, token: &str) -> bool {
    let got = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    ct_eq(got.as_bytes(), token.as_bytes())
}

/// Constant-time byte compare (avoid leaking the token via timing).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Parse `{"id":N,...}` (+ optional `reason`/`prefix`) from the POST body and
/// resolve that confirmation. `allow=false` denies. Returns 404 if the id is
/// unknown/already-resolved (single-use), 400 if the body has no id.
fn resolve_from_body(
    request: &mut Request,
    confirmer: &RemoteConfirmer,
    allow: bool,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    if request.as_reader().read_to_string(&mut body).is_err() {
        return Response::from_string("bad body").with_status_code(400);
    }
    let Some(id) = parse_json_u64(&body, "id") else {
        return Response::from_string("missing id").with_status_code(400);
    };
    let answer = if allow {
        match parse_json_str(&body, "prefix") {
            Some(prefix) if parse_json_str(&body, "scope").as_deref() == Some("remember") => {
                Confirmation::AllowRemember { prefix }
            }
            _ => Confirmation::AllowOnce,
        }
    } else {
        Confirmation::Deny(parse_json_str(&body, "reason").unwrap_or_else(|| "denied".to_string()))
    };
    if confirmer.resolve(id, answer) {
        json("{\"ok\":true}")
    } else {
        Response::from_string("unknown or already-resolved id").with_status_code(404)
    }
}

/// Minimal `"key":<number>` extractor — the bodies are tiny fixed-shape JSON, so a
/// full parser isn't warranted (and avoids a serde_json Value dependency dance).
pub fn parse_json_u64(body: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\"");
    let after = body.split_once(&pat)?.1;
    let after = after.split_once(':')?.1;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Minimal `"key":"<string>"` extractor for the tiny fixed-shape bodies.
pub fn parse_json_str(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let after = body.split_once(&pat)?.1;
    let after = after.split_once(':')?.1.trim_start();
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub(crate) fn unauthorized() -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string("unauthorized").with_status_code(401)
}

pub(crate) fn html(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(content_type("text/html; charset=utf-8"))
}

pub(crate) fn json(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(content_type("application/json"))
}

fn content_type(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).expect("valid header")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_core::{AgentEvent, StopReason};

    #[test]
    fn query_token_matches_only_exact() {
        assert!(query_token_ok("/events?from=0&k=secret", "secret"));
        assert!(query_token_ok("/?k=secret", "secret"));
        assert!(!query_token_ok("/?k=wrong", "secret"));
        assert!(!query_token_ok("/?k=secret", "secretx")); // length mismatch
        assert!(!query_token_ok("/events?from=0", "secret")); // no k at all
                                                              // The token stops at the next & and isn't confused by later params.
        assert!(query_token_ok("/events?k=secret&from=3", "secret"));
    }

    #[test]
    fn parses_json_fields_from_tiny_bodies() {
        assert_eq!(parse_json_u64("{\"id\":7}", "id"), Some(7));
        assert_eq!(parse_json_u64("{ \"id\" : 42 , \"x\":1}", "id"), Some(42));
        assert_eq!(parse_json_u64("{\"scope\":\"once\"}", "id"), None);
        assert_eq!(
            parse_json_str("{\"id\":1,\"reason\":\"nope\"}", "reason").as_deref(),
            Some("nope")
        );
        assert_eq!(
            parse_json_str("{\"scope\":\"remember\",\"prefix\":\"git \"}", "prefix").as_deref(),
            Some("git ")
        );
        assert_eq!(parse_json_str("{\"id\":1}", "reason"), None);
    }

    #[test]
    fn parses_from_query() {
        assert_eq!(parse_from("/events?from=7"), 7);
        assert_eq!(parse_from("/events?from=3&x=1"), 3);
        assert_eq!(parse_from("/events"), 0);
        assert_eq!(parse_from("/events?from=bad"), 0);
    }

    #[test]
    fn events_body_serializes_the_batch() {
        let hub = Hub::new();
        hub.push(AgentEvent::RunStarted {
            task: "t".into(),
            prompt_budget: 100,
        });
        hub.push(AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        let body = events_body(&hub, 0);
        assert!(body.contains("\"next\":2"), "{body}");
        assert!(body.contains("\"done\":true"), "{body}");
        assert!(body.contains("\"type\":\"RunStarted\""), "{body}");

        // From the end: empty batch, still done.
        let tail = events_body(&hub, 2);
        assert!(tail.contains("\"events\":[]"), "{tail}");
        assert!(tail.contains("\"done\":true"), "{tail}");
    }
}
