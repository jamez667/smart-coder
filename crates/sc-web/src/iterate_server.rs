//! The remote **Iterate** server: drive smart-coder's daily-driver in-place edit
//! mode from a phone, over the same `Hub`/`RemoteConfirmer` transport the dashboard
//! uses. This is the server half of the Android remote client (Phase A).
//!
//! Unlike [`crate::serve`] (which starts a fixed run immediately), the iterate server
//! boots **idle**: the workspace is whatever the PC already has open, and the run is
//! started on demand by `POST /run {kind:"iterate", task}`. That matches the phone
//! flow — attach, type a task, Run, watch, approve/deny, stop.
//!
//! The iterate *flavor* (instruction, verify-command detection, the accept-or-revert
//! git safety) is shared with the desktop via the `sc-iterate` crate, so remote and
//! desktop behave identically. Everything network-facing — bearer auth, `/events`
//! replay, `/approve`/`/deny`/`/cancel` — is the proven code from [`crate::server`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use sc_core::{run_agent_observed, AgentConfig, AgentEvent, ToolCallStrategy};
use sc_model::ModelBackend;
use sc_tools::ToolRegistry;
use tiny_http::{Method, Request, Response, Server};

use crate::hub::{Hub, HubSink};
use crate::remote_confirm::RemoteConfirmer;
use crate::server::{
    bearer_ok, events_body, html, json, parse_from, parse_json_str, parse_json_u64, query_token_ok,
    unauthorized, DASHBOARD_HTML,
};

/// Everything needed to start iterate runs on demand. The backend/registry/strategy
/// are built once (they're reused across runs) and the workspace is fixed to whatever
/// the PC opened. `configured_verify` is the user's `--verify` (or None → auto-detect).
pub struct IterateServer<B, A>
where
    B: ModelBackend + Send + Sync + 'static,
    A: ModelBackend + Send + Sync + 'static,
{
    pub backend: Arc<B>,
    pub advisor: Option<Arc<A>>,
    pub registry: Arc<ToolRegistry>,
    pub strategy: Arc<dyn ToolCallStrategy + Send + Sync>,
    pub workspace: PathBuf,
    pub base_config: AgentConfig,
    pub configured_verify: Option<String>,
}

/// The mutable run state shared between the request router and the worker thread.
#[derive(Default)]
struct RunState {
    /// The active run's worker handle, if a run is in flight.
    worker: Option<thread::JoinHandle<()>>,
    /// True once a run has been started (so `/status` reports it and `/run` 409s a
    /// second concurrent run).
    started: bool,
}

/// Serve the idle iterate server on `addr`, returning the bound URL via `on_ready`,
/// then blocking on the request loop until the process is killed. A run is started by
/// `POST /run`; the workspace is `spec.workspace` (whatever the PC opened).
pub fn serve_iterate<B, A>(
    spec: IterateServer<B, A>,
    addr: &str,
    token: &str,
    on_ready: impl FnOnce(String),
) -> std::io::Result<()>
where
    B: ModelBackend + Send + Sync + 'static,
    A: ModelBackend + Send + Sync + 'static,
{
    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    on_ready(format!("http://{}", server.server_addr()));

    let hub = Hub::new();
    let confirmer = RemoteConfirmer::new(hub.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(RunState::default()));
    let spec = Arc::new(spec);

    // Idle until /run: the request loop never exits on `done` (unlike `serve`), because
    // the session outlives a single run — the phone can watch it finish, then the PC
    // operator can start another (a fresh run resets `started`). We serve until killed.
    for mut request in server.incoming_requests() {
        let response = route(
            &hub,
            &confirmer,
            &cancel,
            &state,
            &spec,
            token,
            &mut request,
        );
        let _ = request.respond(response);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn route<B, A>(
    hub: &Hub,
    confirmer: &RemoteConfirmer,
    cancel: &Arc<AtomicBool>,
    state: &Arc<Mutex<RunState>>,
    spec: &Arc<IterateServer<B, A>>,
    token: &str,
    request: &mut Request,
) -> Response<std::io::Cursor<Vec<u8>>>
where
    B: ModelBackend + Send + Sync + 'static,
    A: ModelBackend + Send + Sync + 'static,
{
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
            json(&events_body(hub, parse_from(&url)))
        }
        (Method::Get, "/status") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            let ws = spec
                .workspace
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("(workspace)");
            let running = state.lock().unwrap().started && !hub.is_done();
            json(&format!(
                "{{\"workspace\":{},\"running\":{},\"done\":{},\"events\":{}}}",
                json_str(ws),
                running,
                hub.is_done(),
                hub.len()
            ))
        }
        // Control routes — bearer header required.
        (Method::Post, "/run") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            start_run(hub, confirmer, cancel, state, spec, request)
        }
        (Method::Post, "/approve") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            resolve(request, confirmer, true)
        }
        (Method::Post, "/deny") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            resolve(request, confirmer, false)
        }
        (Method::Post, "/cancel") => {
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

/// `POST /run {kind:"iterate", task}` — start an iterate run in the open workspace.
/// 409 if a run is already active, 400 if the body has no task.
fn start_run<B, A>(
    hub: &Hub,
    confirmer: &RemoteConfirmer,
    cancel: &Arc<AtomicBool>,
    state: &Arc<Mutex<RunState>>,
    spec: &Arc<IterateServer<B, A>>,
    request: &mut Request,
) -> Response<std::io::Cursor<Vec<u8>>>
where
    B: ModelBackend + Send + Sync + 'static,
    A: ModelBackend + Send + Sync + 'static,
{
    let mut body = String::new();
    if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
        return Response::from_string("bad body").with_status_code(400);
    }
    let Some(task) = parse_json_str(&body, "task").filter(|t| !t.trim().is_empty()) else {
        return Response::from_string("missing task").with_status_code(400);
    };

    {
        let mut st = state.lock().unwrap();
        if st.started && !hub.is_done() {
            return Response::from_string("a run is already active").with_status_code(409);
        }
        st.started = true;
    }
    // Fresh cancel flag view for this run.
    cancel.store(false, Ordering::Relaxed);

    // Build the iterate-flavored config: base + confirmer + cancel + the shared overrides.
    let mut agent_cfg = spec.base_config.clone();
    agent_cfg.confirmer = Some(Arc::new(confirmer.clone()));
    agent_cfg.cancel = Some(cancel.clone());
    sc_iterate::apply_iterate_overrides(&mut agent_cfg, &spec.configured_verify, &spec.workspace);

    let instruction = sc_iterate::iterate_instruction(&task, &spec.workspace);
    let dirty_at_start = sc_iterate::git_dirty_files(&spec.workspace);

    let hub_worker = hub.clone();
    let spec = spec.clone();
    let state_worker = state.clone();
    let handle = thread::spawn(move || {
        // Track the files the agent edits, so the shared finish_summary can revert exactly
        // the ones that were clean at start on a failed run.
        let edited: Arc<Mutex<std::collections::BTreeSet<String>>> = Default::default();
        let edited_sink = edited.clone();
        let hub_sink = HubSink::new(hub_worker.clone());
        let sink = crate::hub::FnHubSink::new(move |e: &AgentEvent| {
            if let AgentEvent::ToolCall { tool, arg } = e {
                if matches!(
                    tool.as_str(),
                    "write_file" | "create_file" | "edit_file" | "append_file"
                ) {
                    let p = arg.trim();
                    if !p.is_empty() {
                        edited_sink.lock().unwrap().insert(p.replace('\\', "/"));
                    }
                }
            }
            hub_sink.record_event(e);
        });

        let result = run_agent_observed(
            spec.backend.as_ref(),
            spec.advisor
                .as_ref()
                .map(|a| a.as_ref() as &dyn ModelBackend),
            spec.registry.as_ref(),
            spec.strategy.as_ref(),
            &instruction,
            &spec.workspace,
            &agent_cfg,
            &sink,
        );

        let touched: Vec<String> = edited.lock().unwrap().iter().cloned().collect();
        // The accept-or-revert decision + closing line (shared with the desktop). Push the
        // summary onto the hub as a terminal note so the phone shows it after `Stopped`.
        let summary = match result {
            Ok(report) => {
                let outcome =
                    sc_iterate::finish_summary(&report, &touched, &dirty_at_start, &spec.workspace);
                outcome.summary
            }
            Err(e) => {
                let safe: Vec<String> = touched
                    .iter()
                    .filter(|f| !dirty_at_start.contains(*f))
                    .cloned()
                    .collect();
                sc_iterate::git_revert_files(&spec.workspace, &safe);
                format!(
                    "iterate failed: {e} (reverted {} clean file(s))",
                    safe.len()
                )
            }
        };
        hub_worker.push(AgentEvent::ToolResult {
            summary: summary.clone(),
            full: summary,
            is_error: false,
        });
        // Emit a terminal Stopped so the hub latches `done` and the phone's poll loop
        // flips out of "running" (returning the Send button). Without this the run never
        // *looks* finished to a client even though it is.
        hub_worker.push(AgentEvent::Stopped {
            reason: sc_core::StopReason::Finished,
        });
        // Allow a subsequent /run once this one is fully wrapped up.
        state_worker.lock().unwrap().started = false;
    });

    state.lock().unwrap().worker = Some(handle);
    json("{\"ok\":true}")
}

/// `POST /approve|/deny {id}` — resolve a pending confirmation by id.
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

/// Minimal JSON string encoder for the small `/status` body (escapes quote+backslash).
fn json_str(s: &str) -> String {
    let esc = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{esc}\"")
}
