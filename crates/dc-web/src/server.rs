//! The local web dashboard server (spec 01/06).
//!
//! Runs the agent on a worker thread feeding a [`Hub`]; serves the embedded
//! dashboard page and an incremental `/events?from=N` JSON feed the browser polls.
//! A blocking [`tiny_http`] server keeps us off an async runtime, consistent with
//! the rest of the codebase.

use std::path::PathBuf;
use std::thread;

use dc_core::{run_agent_observed, AgentConfig, AgentReport, ToolCallStrategy};
use dc_model::ModelBackend;
use dc_tools::ToolRegistry;
use tiny_http::{Header, Method, Response, Server};

use crate::hub::{Hub, HubSink};

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

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
    spec: WebRun<B, A>,
    addr: &str,
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
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let response = route(&hub, request.method(), &url);
        let _ = request.respond(response);
        // Stop once the run finished and the browser has read the terminal event.
        if hub.is_done() && url.starts_with("/events") && parse_from(&url) >= hub.len() {
            break;
        }
    }

    Ok(worker.join().ok().and_then(|r| r.ok()))
}

fn route(hub: &Hub, method: &Method, url: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match (method, url) {
        (Method::Get, u) if u == "/" || u.starts_with("/index") => html(DASHBOARD_HTML),
        (Method::Get, u) if u.starts_with("/events") => json(&events_body(hub, parse_from(u))),
        _ => Response::from_string("not found").with_status_code(404),
    }
}

fn html(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(content_type("text/html; charset=utf-8"))
}

fn json(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(content_type("application/json"))
}

fn content_type(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).expect("valid header")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_core::{AgentEvent, StopReason};

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
