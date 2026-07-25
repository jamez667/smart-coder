//! A local dashboard for a compliance evidence pack.
//!
//! Unlike the run dashboards in this crate there is no event stream to poll:
//! an audit is a batch job that produces one artifact. So the server holds the
//! most recent [`EvidencePack`] behind a mutex, serves it as JSON at `/report`,
//! and re-runs the audit on `POST /audit`.
//!
//! The first audit runs *before* the server binds, so the page never has to
//! render a "not ready yet" state — opening the URL always shows a real pack.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tiny_http::{Method, Request, Response, Server};

use sc_comply::collector::ComplyOptions;
use sc_comply::engine::audit;
use sc_comply::evidence::EvidencePack;
use sc_comply::pack::Pack;

use crate::server::{bearer_ok, html, json, query_token_ok, unauthorized};

pub(crate) const COMPLY_DASHBOARD_HTML: &str = include_str!("comply_dashboard.html");

/// One framework available in the dashboard.
pub struct FrameworkEntry {
    /// Short name used in the `?framework=` query, e.g. `"iso27001"`.
    pub name: String,
    /// The parsed, validated pack.
    pub pack: Pack,
}

/// Everything needed to run and re-run audits.
///
/// Holds every framework the dashboard offers rather than a single pack: the
/// interesting question for most users is not "how do we score against SOC 2"
/// but "where do our frameworks overlap, and what is genuinely missing".
pub struct ComplyRun {
    /// The workspace under audit.
    pub workspace: PathBuf,
    /// Frameworks in display order. The first is the default view.
    pub frameworks: Vec<FrameworkEntry>,
    pub options: ComplyOptions,
}

impl ComplyRun {
    /// Audit one framework by name.
    fn run_one(&self, name: &str) -> sc_proto::Result<EvidencePack> {
        let entry = self
            .frameworks
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| sc_proto::DcError::Comply(format!("unknown framework {name:?}")))?;
        audit(&self.workspace, &entry.pack, &self.options)
    }

    fn default_name(&self) -> Option<&str> {
        self.frameworks.first().map(|f| f.name.as_str())
    }
}

/// Cached audit results, keyed by framework name.
///
/// Audits are run lazily and memoized: auditing ten frameworks up front would
/// re-scan the tree ten times before the page could load, and most users look at
/// two or three.
type Cache = Mutex<HashMap<String, EvidencePack>>;

/// Bind `addr`, audit the default framework, and serve until the process is
/// killed. Returns the bound URL via `on_ready` before blocking.
pub fn serve_comply(
    spec: ComplyRun,
    addr: &str,
    token: &str,
    on_ready: impl FnOnce(String),
) -> std::io::Result<()> {
    let Some(default) = spec.default_name().map(|s| s.to_string()) else {
        return Err(std::io::Error::other("no frameworks configured"));
    };

    // Audit the default up front: if the workspace or pack is bad we fail here,
    // loudly, rather than binding a port and serving an error page.
    let initial = spec
        .run_one(&default)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    let url = format!("http://{}", server.server_addr());
    on_ready(url);

    let spec = Arc::new(spec);
    let cache: Arc<Cache> = Arc::new(Mutex::new(HashMap::from([(default, initial)])));

    for mut request in server.incoming_requests() {
        let response = route(&spec, &cache, token, &mut request);
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
    cache: &Arc<Cache>,
    token: &str,
    request: &mut Request,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");

    // An EMPTY token means auth is deliberately off (`--no-token`). The server
    // is bound to 127.0.0.1 either way, and this dashboard is read-only over a
    // local audit of the user's own workspace — so for a local run the 64-char
    // token in the URL is friction that buys little. It must be opted into
    // explicitly; `mint_token()` never returns empty, so this cannot happen by
    // accident.
    let open = token.is_empty();
    let get_ok = |u: &str| open || query_token_ok(u, token);

    match (&method, path) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            if !get_ok(&url) {
                return unauthorized();
            }
            html(COMPLY_DASHBOARD_HTML)
        }

        // The frameworks on offer, so the page can build its selector.
        (Method::Get, "/frameworks") => {
            if !get_ok(&url) {
                return unauthorized();
            }
            let items: Vec<String> = spec
                .frameworks
                .iter()
                .map(|f| {
                    format!(
                        "{{\"name\":{},\"title\":{},\"controls\":{}}}",
                        json_string(&f.name),
                        json_string(&f.pack.framework.name),
                        f.pack.controls.len()
                    )
                })
                .collect();
            json(&format!("{{\"frameworks\":[{}]}}", items.join(",")))
        }

        // One framework's evidence pack, audited on first request and memoized.
        (Method::Get, "/report") => {
            if !get_ok(&url) {
                return unauthorized();
            }
            let name = match framework_param(&url, spec) {
                Some(n) => n,
                None => return server_error("no frameworks configured"),
            };
            match cached_report(spec, cache, &name) {
                Ok(body) => json(&body),
                Err(e) => server_error(&e),
            }
        }

        // Re-audit one framework, replacing its cached result.
        (Method::Post, "/audit") => {
            // The bearer header is the CSRF defense, so it is checked even
            // harder than the GETs — but with auth explicitly off there is no
            // token to present.
            if !open && !bearer_ok(request, token) {
                return unauthorized();
            }
            let name = match framework_param(&url, spec) {
                Some(n) => n,
                None => return server_error("no frameworks configured"),
            };
            match spec.run_one(&name) {
                Ok(fresh) => {
                    let body = match sc_comply::report::json::render(&fresh) {
                        Ok(b) => b,
                        Err(e) => return server_error(&e.to_string()),
                    };
                    if let Ok(mut map) = cache.lock() {
                        map.insert(name, fresh);
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

/// The `framework=` query param, falling back to the default.
///
/// An unknown name falls back too rather than erroring: a stale bookmark should
/// show something useful, and `run_one` rejects it anyway if it is truly absent.
fn framework_param(url: &str, spec: &ComplyRun) -> Option<String> {
    let requested = url
        .split_once("framework=")
        .map(|(_, rest)| rest.split('&').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty());

    match requested {
        Some(name) if spec.frameworks.iter().any(|f| f.name == name) => Some(name),
        _ => spec.default_name().map(|s| s.to_string()),
    }
}

/// Return a framework's report, auditing it on first request.
fn cached_report(
    spec: &Arc<ComplyRun>,
    cache: &Arc<Cache>,
    name: &str,
) -> std::result::Result<String, String> {
    // Fast path: already audited.
    if let Ok(map) = cache.lock() {
        if let Some(pack) = map.get(name) {
            return sc_comply::report::json::render(pack).map_err(|e| e.to_string());
        }
    }

    // Audit outside the lock — a ten-second scan must not block other
    // frameworks' requests behind it.
    let fresh = spec.run_one(name).map_err(|e| e.to_string())?;
    let body = sc_comply::report::json::render(&fresh).map_err(|e| e.to_string())?;
    if let Ok(mut map) = cache.lock() {
        map.insert(name.to_string(), fresh);
    }
    Ok(body)
}

/// Minimal JSON string escaping. Framework names and titles are pack-authored,
/// so they are trusted-but-quoted rather than sanitized.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
            frameworks: vec![FrameworkEntry {
                name: "soc2".to_string(),
                pack: Pack::from_toml_str(PACK).expect("pack parses"),
            }],
            options: ComplyOptions::default(),
        }
    }

    /// A spec with two frameworks, for the selector behaviour.
    fn multi_spec(root: &std::path::Path) -> ComplyRun {
        ComplyRun {
            workspace: root.to_path_buf(),
            frameworks: vec![
                FrameworkEntry {
                    name: "soc2".to_string(),
                    pack: sc_comply::registry::load_shipped("soc2").expect("soc2"),
                },
                FrameworkEntry {
                    name: "iso27001".to_string(),
                    pack: sc_comply::registry::load_shipped("iso27001").expect("iso"),
                },
            ],
            options: ComplyOptions::default(),
        }
    }

    #[test]
    fn framework_param_defaults_to_the_first() {
        let root = temp_repo("param-default");
        let spec = multi_spec(&root);
        assert_eq!(
            framework_param("/report?k=x", &spec).as_deref(),
            Some("soc2")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn framework_param_honours_a_known_name() {
        let root = temp_repo("param-known");
        let spec = multi_spec(&root);
        assert_eq!(
            framework_param("/report?framework=iso27001&k=x", &spec).as_deref(),
            Some("iso27001")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_framework_falls_back_rather_than_erroring() {
        // A stale bookmark should still show something useful.
        let root = temp_repo("param-unknown");
        let spec = multi_spec(&root);
        assert_eq!(
            framework_param("/report?framework=nope&k=x", &spec).as_deref(),
            Some("soc2")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn each_framework_audits_independently() {
        let root = temp_repo("multi-audit");
        std::fs::write(root.join("README.md"), "hi\n").expect("write");
        let spec = multi_spec(&root);

        let soc2 = spec.run_one("soc2").expect("soc2 audits");
        let iso = spec.run_one("iso27001").expect("iso audits");
        assert_ne!(soc2.framework.id, iso.framework.id);
        assert_eq!(soc2.score.errors, 0);
        assert_eq!(iso.score.errors, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_framework_name_is_an_error_when_audited_directly() {
        let root = temp_repo("multi-unknown");
        let spec = multi_spec(&root);
        assert!(spec.run_one("nope").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_token_means_auth_is_off_but_only_when_empty() {
        // The whole safety argument for --no-token rests on mint_token() never
        // returning empty, so a real token can never accidentally open the
        // server. Pin that here rather than trusting the reader to check.
        assert!(!crate::mint_token().is_empty());
        assert_eq!(crate::mint_token().len(), 64);
    }

    #[test]
    fn json_string_escapes_quotes_and_controls() {
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
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

        let pack = spec_for(&root).run_one("soc2").expect("audit");
        assert_eq!(pack.schema_version, 1);
        assert!(!pack.controls.is_empty());
        assert_eq!(pack.score.errors, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_served_report_round_trips_as_json() {
        let root = temp_repo("json");
        std::fs::write(root.join("README.md"), "hi\n").expect("write");

        let pack = spec_for(&root).run_one("soc2").expect("audit");
        let body = sc_comply::report::json::render(&pack).expect("render");
        let back: EvidencePack = serde_json::from_str(&body).expect("parse");
        assert_eq!(back, pack);

        let _ = std::fs::remove_dir_all(&root);
    }
}
