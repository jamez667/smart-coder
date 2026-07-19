//! The **session mirror** server: attach a remote client (the phone) to a run that
//! ANOTHER process — the `sc-win` desktop app — already owns. Unlike [`crate::serve`]
//! and [`crate::serve_iterate`], this server does **not** spawn an agent. It just
//! serves a shared [`Hub`] the desktop tees its live events (agent activity + chat)
//! into, and collects inbound commands (chat messages, cancel) the desktop drains and
//! applies to its own running session. Approve/deny resolve through a shared
//! [`RemoteConfirmer`] exactly as the other servers do.
//!
//! This is the "Claude Code remote" shape: the phone mirrors and drives the *actual*
//! desktop session, rather than kicking off a separate headless run.
//!
//! The transport (bearer auth, `/events` replay, the router helpers) is reused verbatim
//! from [`crate::server`]; only the "no run, tee + inbound queue" wiring is new.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tiny_http::{Method, Request, Response, Server};

use crate::hub::Hub;
use crate::remote_confirm::RemoteConfirmer;
use crate::server::{
    bearer_ok, events_body, html, json, parse_from, parse_json_str, parse_json_u64, query_token_ok,
    unauthorized, DASHBOARD_HTML,
};

/// A command a remote client sent, for the desktop to apply to its live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundCmd {
    /// The phone sent a chat message — the desktop routes it into `send_chat()`.
    Chat(String),
    /// The phone hit stop — the desktop calls `session.cancel()`.
    Cancel,
    /// The phone picked a recent project to switch to (a path from the recents list the
    /// desktop published). The desktop validates it against that list and opens it.
    Open(String),
}

/// The projects the mirror advertises: the current workspace and the recent list the phone
/// can switch between. Set by the desktop via [`RemoteMirror::set_projects`].
#[derive(Debug, Clone, Default)]
struct Projects {
    current: Option<String>,
    recents: Vec<String>,
}

/// The shared handle between the desktop `App` and the mirror server thread. The desktop
/// pushes live events out (via [`RemoteMirror::push`] / [`RemoteMirror::confirmer`]) and
/// drains inbound commands (via [`RemoteMirror::drain_inbound`]); the server reads the
/// `Hub` for `/events` and fills the inbound queue from POSTs. Clone is cheap (all `Arc`).
#[derive(Clone)]
pub struct RemoteMirror {
    hub: Hub,
    confirmer: RemoteConfirmer,
    cancel: Arc<AtomicBool>,
    inbound: Arc<Mutex<VecDeque<InboundCmd>>>,
    projects: Arc<Mutex<Projects>>,
}

impl Default for RemoteMirror {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteMirror {
    pub fn new() -> Self {
        let hub = Hub::new();
        Self {
            confirmer: RemoteConfirmer::new(hub.clone()),
            hub,
            cancel: Arc::new(AtomicBool::new(false)),
            inbound: Arc::new(Mutex::new(VecDeque::new())),
            projects: Arc::new(Mutex::new(Projects::default())),
        }
    }

    /// Set the current workspace + recents the phone can see and switch between. The desktop
    /// calls this whenever the open project changes.
    pub fn set_projects(&self, current: Option<String>, recents: Vec<String>) {
        *self.projects.lock().unwrap() = Projects { current, recents };
    }

    /// Broadcast one event to connected clients (the desktop's `pump()` tee calls this
    /// for every agent/chat event).
    pub fn push(&self, event: sc_core::AgentEvent) {
        self.hub.push(event);
    }

    /// Register a pending confirm the desktop is showing on its gate bar, so a remote
    /// client can approve/deny it. The desktop passes the `Sender<Confirmation>` it
    /// already holds; the returned `id` correlates the later `/approve`/`/deny`. This
    /// announces `ConfirmPending{id,..}` to remote clients via the hub.
    pub fn register_confirm(
        &self,
        command: &str,
        reason: &str,
        reply: std::sync::mpsc::Sender<sc_core::Confirmation>,
    ) -> u64 {
        self.confirmer.register(command, reason, reply)
    }

    /// The underlying confirmer (for `deny_all` on teardown, etc.).
    pub fn confirmer(&self) -> &RemoteConfirmer {
        &self.confirmer
    }

    /// Whether a remote client has requested cancellation since the last check.
    pub fn take_cancel(&self) -> bool {
        self.cancel.swap(false, Ordering::Relaxed)
    }

    /// Drain the inbound commands the desktop should apply this tick (chat / cancel).
    pub fn drain_inbound(&self) -> Vec<InboundCmd> {
        let mut q = self.inbound.lock().unwrap();
        q.drain(..).collect()
    }

    fn push_inbound(&self, cmd: InboundCmd) {
        self.inbound.lock().unwrap().push_back(cmd);
    }
}

/// Serve the mirror on `addr`, returning the bound URL via `on_ready`, then blocking on
/// the request loop until the process is killed. `mirror` is shared with the desktop app.
pub fn serve_mirror(
    mirror: RemoteMirror,
    addr: &str,
    token: &str,
    on_ready: impl FnOnce(String),
) -> std::io::Result<()> {
    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    on_ready(format!("http://{}", server.server_addr()));

    for mut request in server.incoming_requests() {
        let response = route(&mirror, token, &mut request);
        let _ = request.respond(response);
    }
    Ok(())
}

fn route(
    mirror: &RemoteMirror,
    token: &str,
    request: &mut Request,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");
    match (&method, path) {
        // Read routes — token in the query string.
        (Method::Get, "/") | (Method::Get, "/index") | (Method::Get, "/index.html") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            html(DASHBOARD_HTML)
        }
        (Method::Get, "/events") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            json(&events_body(&mirror.hub, parse_from(&url)))
        }
        (Method::Get, "/status") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            let cur = mirror.projects.lock().unwrap().current.clone();
            json(&format!(
                "{{\"mode\":\"mirror\",\"events\":{},\"workspace\":{}}}",
                mirror.hub.len(),
                cur.map(|s| json_str(&s)).unwrap_or_else(|| "null".into()),
            ))
        }
        (Method::Get, "/projects") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            let p = mirror.projects.lock().unwrap();
            let cur = p
                .current
                .as_ref()
                .map(|s| json_str(s))
                .unwrap_or_else(|| "null".into());
            let items: Vec<String> = p
                .recents
                .iter()
                .map(|path| {
                    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
                    format!("{{\"name\":{},\"path\":{}}}", json_str(name), json_str(path))
                })
                .collect();
            json(&format!(
                "{{\"current\":{cur},\"projects\":[{}]}}",
                items.join(",")
            ))
        }
        // Control routes — bearer header required.
        (Method::Post, "/chat") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            let mut body = String::new();
            if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
                return Response::from_string("bad body").with_status_code(400);
            }
            match parse_json_str(&body, "text").filter(|t| !t.trim().is_empty()) {
                Some(text) => {
                    mirror.push_inbound(InboundCmd::Chat(text));
                    json("{\"ok\":true}")
                }
                None => Response::from_string("missing text").with_status_code(400),
            }
        }
        (Method::Post, "/open") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            let mut body = String::new();
            if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
                return Response::from_string("bad body").with_status_code(400);
            }
            match parse_json_str(&body, "path").filter(|p| !p.trim().is_empty()) {
                Some(path) => {
                    mirror.push_inbound(InboundCmd::Open(path));
                    json("{\"ok\":true}")
                }
                None => Response::from_string("missing path").with_status_code(400),
            }
        }
        (Method::Post, "/approve") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            resolve(request, &mirror.confirmer, true)
        }
        (Method::Post, "/deny") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            resolve(request, &mirror.confirmer, false)
        }
        (Method::Post, "/cancel") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            mirror.cancel.store(true, Ordering::Relaxed);
            mirror.push_inbound(InboundCmd::Cancel);
            mirror.confirmer.deny_all("run cancelled");
            json("{\"ok\":true}")
        }
        _ => Response::from_string("not found").with_status_code(404),
    }
}

/// `POST /approve|/deny {id}` — resolve a pending confirmation by id (shared with the
/// other servers' shape).
fn resolve(
    request: &mut Request,
    confirmer: &RemoteConfirmer,
    allow: bool,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
        return Response::from_string("bad body").with_status_code(400);
    }
    let Some(id) = parse_json_u64(&body, "id") else {
        return Response::from_string("missing id").with_status_code(400);
    };
    let answer = if allow {
        match parse_json_str(&body, "prefix") {
            Some(prefix) if parse_json_str(&body, "scope").as_deref() == Some("remember") => {
                sc_core::Confirmation::AllowRemember { prefix }
            }
            _ => sc_core::Confirmation::AllowOnce,
        }
    } else {
        sc_core::Confirmation::Deny(
            parse_json_str(&body, "reason").unwrap_or_else(|| "denied".to_string()),
        )
    };
    if confirmer.resolve(id, answer) {
        json("{\"ok\":true}")
    } else {
        Response::from_string("unknown or already-resolved id").with_status_code(404)
    }
}

/// Minimal JSON string encoder (escapes `\` and `"`) for the small status/projects bodies.
fn json_str(s: &str) -> String {
    let esc = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{esc}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_queue_roundtrips_chat_and_cancel() {
        let m = RemoteMirror::new();
        m.push_inbound(InboundCmd::Chat("hello".into()));
        m.push_inbound(InboundCmd::Cancel);
        let got = m.drain_inbound();
        assert_eq!(got, vec![InboundCmd::Chat("hello".into()), InboundCmd::Cancel]);
        // Draining again yields nothing.
        assert!(m.drain_inbound().is_empty());
    }

    #[test]
    fn push_reaches_the_hub_for_replay() {
        let m = RemoteMirror::new();
        m.push(sc_core::AgentEvent::ChatMessage {
            role: "agent".into(),
            text: "hi from desktop".into(),
        });
        let body = events_body(&m.hub, 0);
        assert!(body.contains("\"ChatMessage\""), "{body}");
        assert!(body.contains("hi from desktop"), "{body}");
    }

    #[test]
    fn cancel_flag_is_one_shot() {
        let m = RemoteMirror::new();
        assert!(!m.take_cancel());
        m.cancel.store(true, Ordering::Relaxed);
        assert!(m.take_cancel());
        assert!(!m.take_cancel(), "cancel is consumed once");
    }
}
