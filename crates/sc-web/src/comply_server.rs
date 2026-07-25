//! A local dashboard for a compliance evidence pack.
//!
//! Unlike the run dashboards in this crate there is no event stream to poll:
//! an audit is a batch job that produces one artifact. So the server holds the
//! most recent [`EvidencePack`] behind a mutex, serves it as JSON at `/report`,
//! and re-runs the audit on `POST /audit`.
//!
//! The first audit runs *before* the server binds, so the page never has to
//! render a "not ready yet" state — opening the URL always shows a real pack.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tiny_http::{Method, Request, Response, Server};

use sc_comply::collector::ComplyOptions;
use sc_comply::engine::audit;
use sc_comply::evidence::EvidencePack;
use sc_comply::pack::Pack;

use crate::server::{bearer_ok, html, json, query_token_ok, unauthorized};

pub(crate) const COMPLY_DASHBOARD_HTML: &str = include_str!("comply_dashboard.html");

/// Everything needed to run and re-run an audit.
pub struct ComplyRun {
    /// The workspace under audit.
    pub workspace: PathBuf,
    /// The framework pack, already parsed and validated.
    pub pack: Pack,
    pub options: ComplyOptions,
}

impl ComplyRun {
    /// Run the audit once. Errors are surfaced to the caller rather than
    /// swallowed — a failed audit must not render as an empty clean report.
    fn run(&self) -> sc_proto::Result<EvidencePack> {
        audit(&self.workspace, &self.pack, &self.options)
    }
}

/// Bind `addr`, run the audit once, and serve the dashboard until the process
/// is killed. Returns the bound URL via `on_ready` before blocking.
pub fn serve_comply(
    spec: ComplyRun,
    addr: &str,
    token: &str,
    on_ready: impl FnOnce(String),
) -> std::io::Result<()> {
    // Audit first: if the pack or workspace is bad we fail here, loudly, rather
    // than binding a port and serving an error page.
    let initial = spec
        .run()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    let url = format!("http://{}", server.server_addr());
    on_ready(url);

    let spec = Arc::new(spec);
    let current = Arc::new(Mutex::new(initial));

    for mut request in server.incoming_requests() {
        let response = route(&spec, &current, token, &mut request);
        let _ = request.respond(response);
    }
    Ok(())
}

/// Route one request. Mirrors the other servers in this crate: the token rides
/// in the query for GETs (so a plain URL or QR hands it over) and in an
/// `Authorization: Bearer` header for the state-changing POST, which is the
/// CSRF defense.
fn route(
    spec: &Arc<ComplyRun>,
    current: &Arc<Mutex<EvidencePack>>,
    token: &str,
    request: &mut Request,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");

    match (&method, path) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            html(COMPLY_DASHBOARD_HTML)
        }

        // The current evidence pack, exactly as `report::json` renders it.
        (Method::Get, "/report") => {
            if !query_token_ok(&url, token) {
                return unauthorized();
            }
            match current.lock() {
                Ok(pack) => match sc_comply::report::json::render(&pack) {
                    Ok(body) => json(&body),
                    Err(e) => server_error(&e.to_string()),
                },
                Err(_) => server_error("evidence pack lock poisoned"),
            }
        }

        // Re-run the audit and return the fresh pack.
        (Method::Post, "/audit") => {
            if !bearer_ok(request, token) {
                return unauthorized();
            }
            match spec.run() {
                Ok(fresh) => {
                    let body = match sc_comply::report::json::render(&fresh) {
                        Ok(b) => b,
                        Err(e) => return server_error(&e.to_string()),
                    };
                    if let Ok(mut slot) = current.lock() {
                        *slot = fresh;
                    }
                    json(&body)
                }
                // An audit that fails is a tool failure and must say so, rather
                // than leaving the page showing a stale pack as if it were new.
                Err(e) => server_error(&e.to_string()),
            }
        }

        _ => Response::from_string("not found").with_status_code(404),
    }
}

fn server_error(msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(format!("audit error: {msg}")).with_status_code(500)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACK: &str = include_str!("../../sc-comply/packs/soc2-tsc.toml");

    fn temp_repo(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("sc-web-comply-{tag}-{nanos}"));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }

    fn spec_for(root: &std::path::Path) -> ComplyRun {
        ComplyRun {
            workspace: root.to_path_buf(),
            pack: Pack::from_toml_str(PACK).expect("pack parses"),
            options: ComplyOptions::default(),
        }
    }

    #[test]
    fn dashboard_html_is_embedded_and_self_contained() {
        // Matches the other dashboards: one file, no external fetches, no build
        // step. An external <script src> would break the offline/Tailscale case.
        assert!(COMPLY_DASHBOARD_HTML.contains("<!doctype html>"));
        assert!(!COMPLY_DASHBOARD_HTML.contains("<script src="));
        assert!(!COMPLY_DASHBOARD_HTML.contains("<link rel=\"stylesheet\""));
        assert!(!COMPLY_DASHBOARD_HTML.contains("https://"));
    }

    #[test]
    fn dashboard_puts_scope_before_the_score() {
        // The same honesty-of-layout property the Markdown renderer asserts.
        let scope = COMPLY_DASHBOARD_HTML
            .find("scope &amp; limitations")
            .expect("scope section");
        let score = COMPLY_DASHBOARD_HTML
            .find("id=\"score\"")
            .expect("score section");
        assert!(scope < score, "the numbers must not precede the caveats");
    }

    #[test]
    fn dashboard_renders_evidence_with_text_content_only() {
        // Evidence excerpts are arbitrary file contents; innerHTML would make a
        // committed <script> tag executable in the auditor's browser.
        assert!(!COMPLY_DASHBOARD_HTML.contains("innerHTML"));
        assert!(COMPLY_DASHBOARD_HTML.contains("textContent"));
    }

    #[test]
    fn audit_runs_and_produces_a_pack() {
        let root = temp_repo("run");
        std::fs::write(root.join("README.md"), "hi\n").expect("write");

        let pack = spec_for(&root).run().expect("audit");
        assert_eq!(pack.schema_version, 1);
        assert!(!pack.controls.is_empty());
        assert_eq!(pack.score.errors, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_served_report_round_trips_as_json() {
        let root = temp_repo("json");
        std::fs::write(root.join("README.md"), "hi\n").expect("write");

        let pack = spec_for(&root).run().expect("audit");
        let body = sc_comply::report::json::render(&pack).expect("render");
        let back: EvidencePack = serde_json::from_str(&body).expect("parse");
        assert_eq!(back, pack);

        let _ = std::fs::remove_dir_all(&root);
    }
}
