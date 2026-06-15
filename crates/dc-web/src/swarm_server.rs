//! The swarm web dashboard (spec 06/08): runs a swarm on a worker thread feeding
//! a [`SwarmHub`], and serves the embedded swarm dashboard + an incremental
//! `/events?from=N` feed of orchestrator-level [`SwarmEvent`]s (task board,
//! worker status, integration results).
//!
//! Mirrors [`crate::serve`] (the single-agent dashboard) but over the
//! `dc_swarm` orchestrator instead of one agent loop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use dc_model::ModelBackend;
use dc_swarm::{run_swarm, SwarmConfig, SwarmEvent, SwarmReport, SwarmSink};
use tiny_http::{Header, Method, Response, Server};

use crate::server::parse_from;

const SWARM_HTML: &str = include_str!("swarm_dashboard.html");

/// Everything needed to drive a swarm behind the dashboard. Backends are owned
/// (moved to the worker thread). Workers + advisor must be `Sync` (shared across
/// the swarm's parallel worker threads).
pub struct WebSwarm<O, W, A>
where
    O: ModelBackend + Send + 'static,
    W: ModelBackend + Send + Sync + 'static,
    A: ModelBackend + Send + Sync + 'static,
{
    pub orchestrator: O,
    pub worker: W,
    pub advisor: Option<A>,
    pub task: String,
    pub repo_overview: String,
    pub workspace: PathBuf,
    pub config: SwarmConfig,
}

/// A thread-safe broadcast buffer of swarm events the browser polls.
#[derive(Clone, Default)]
pub struct SwarmHub {
    inner: Arc<Mutex<SwarmHubInner>>,
}

#[derive(Default)]
struct SwarmHubInner {
    events: Vec<SwarmEvent>,
    done: bool,
}

impl SwarmHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, event: SwarmEvent) {
        let mut g = self.inner.lock().unwrap();
        if matches!(event, SwarmEvent::SwarmDone { .. }) {
            g.done = true;
        }
        g.events.push(event);
    }

    fn since(&self, from: usize) -> (Vec<SwarmEvent>, usize, bool) {
        let g = self.inner.lock().unwrap();
        let start = from.min(g.events.len());
        (g.events[start..].to_vec(), g.events.len(), g.done)
    }

    fn is_done(&self) -> bool {
        self.inner.lock().unwrap().done
    }

    fn len(&self) -> usize {
        self.inner.lock().unwrap().events.len()
    }
}

/// A [`SwarmSink`] feeding a [`SwarmHub`].
struct SwarmHubSink {
    hub: SwarmHub,
}
impl SwarmSink for SwarmHubSink {
    fn record(&self, event: &SwarmEvent) {
        self.hub.push(event.clone());
    }
}

fn events_body(hub: &SwarmHub, from: usize) -> String {
    let (events, next, done) = hub.since(from);
    let json = serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string());
    format!("{{\"events\":{json},\"next\":{next},\"done\":{done}}}")
}

/// Serve the swarm dashboard while a swarm runs. Returns the bound URL via
/// `on_ready`, then blocks until the run ends and the browser has drained it.
pub fn serve_swarm<O, W, A>(
    spec: WebSwarm<O, W, A>,
    addr: &str,
    on_ready: impl FnOnce(String),
) -> std::io::Result<Option<SwarmReport>>
where
    O: ModelBackend + Send + 'static,
    W: ModelBackend + Send + Sync + 'static,
    A: ModelBackend + Send + Sync + 'static,
{
    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    on_ready(format!("http://{}", server.server_addr()));

    let hub = SwarmHub::new();
    let run_hub = hub.clone();
    let worker = thread::spawn(move || {
        let sink = SwarmHubSink { hub: run_hub };
        run_swarm(
            &spec.orchestrator,
            &spec.worker,
            spec.advisor
                .as_ref()
                .map(|a| a as &(dyn ModelBackend + Sync)),
            &spec.task,
            &spec.repo_overview,
            &spec.workspace,
            &spec.config,
            &sink,
        )
    });

    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let response = route(&hub, request.method(), &url);
        let _ = request.respond(response);
        if hub.is_done() && url.starts_with("/events") && parse_from(&url) >= hub.len() {
            break;
        }
    }

    Ok(worker.join().ok())
}

fn route(hub: &SwarmHub, method: &Method, url: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match (method, url) {
        (Method::Get, u) if u == "/" || u.starts_with("/index") => html(SWARM_HTML),
        (Method::Get, u) if u.starts_with("/events") => json(&events_body(hub, parse_from(u))),
        _ => Response::from_string("not found").with_status_code(404),
    }
}

fn html(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(ctype("text/html; charset=utf-8"))
}
fn json(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(ctype("application/json"))
}
fn ctype(v: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).expect("valid header")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_buffers_swarm_events_and_marks_done() {
        let hub = SwarmHub::new();
        hub.push(SwarmEvent::Decomposed {
            subtasks: vec!["a".into(), "b".into()],
        });
        assert!(!hub.is_done());
        hub.push(SwarmEvent::SwarmDone {
            done: 2,
            failed: 0,
            all_done: true,
        });
        assert!(hub.is_done());
        let body = events_body(&hub, 0);
        assert!(body.contains("\"type\":\"Decomposed\""), "{body}");
        assert!(body.contains("\"done\":true"), "{body}");
        assert!(body.contains("\"next\":2"), "{body}");
    }

    #[test]
    fn since_is_incremental() {
        let hub = SwarmHub::new();
        hub.push(SwarmEvent::WorkerStarted {
            subtask: "a".into(),
            goal: "do a".into(),
            prompt: "Task: do a".into(),
        });
        let (_, next, _) = hub.since(0);
        let (batch2, _, _) = hub.since(next);
        assert!(batch2.is_empty());
    }
}
