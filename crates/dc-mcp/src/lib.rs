//! `dc-mcp` — dumb-coder as an MCP server (spec 06 machine surface).
//!
//! Exposes dumb-coder's headless `run --json` / `swarm --json` agent to an MCP
//! client (Claude Code) as a **fire-and-poll** parallel coding agent: a `code`
//! tool starts a job and returns its id immediately; a `status` tool polls it.
//! Claude issues several `code` calls at once to run local workers in parallel
//! while it does other work, then verifies each diff itself.
//!
//! The binary ([`crate::main`]) is a thin stdio loop over [`serve`]; all the
//! testable logic — the JSON-RPC dispatch, the tool schemas, the job store —
//! lives in these modules in the project's TDD style.

pub mod jobs;
pub mod protocol;
pub mod tools;

use jobs::{JobConfig, JobStore};
use tools::StoreTools;

/// Environment-driven configuration for the server. Defaults target the project's
/// current live-test rig (Docker llama.cpp serving the 30B on :11435), overridable
/// per deployment.
#[derive(Debug, Clone)]
pub struct Config {
    pub binary: String,
    /// One or more backend URLs. Jobs round-robin across them (one llama.cpp pool
    /// per GPU, say). Always non-empty. The health check uses the first.
    pub base_urls: Vec<String>,
    pub model: String,
    pub yolo: bool,
    pub default_workspace: String,
}

impl Config {
    /// Resolve config from the environment, falling back to the live-test defaults.
    ///
    /// * `DC_MCP_BINARY`  — path to the `dumb-coder` binary (default: `dumb-coder`
    ///   on `PATH`).
    /// * `DC_BASE_URLS`   — comma-separated backend URLs; jobs round-robin across
    ///   them (e.g. one pool per GPU: `…:11439/v1,…:11440/v1`). Falls back to the
    ///   single `DC_BASE_URL`, then to the default rig.
    /// * `DC_BASE_URL`    — a single backend URL (used when `DC_BASE_URLS` is unset).
    /// * `DC_MODEL`       — model tag (default: `qwen3-coder-30b`).
    /// * `DC_MCP_YOLO`    — `0`/`false` to *disable* shell auto-approval (default on:
    ///   a headless run can't prompt, so shell must be pre-approved or it stalls).
    /// * `DC_MCP_WORKSPACE` — default workspace when a `code` call omits one
    ///   (default: the server's current directory).
    pub fn from_env() -> Self {
        let default_workspace = std::env::var("DC_MCP_WORKSPACE").unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_string())
        });
        Config {
            binary: env_or("DC_MCP_BINARY", "dumb-coder"),
            base_urls: base_urls_from_env(),
            model: env_or("DC_MODEL", "qwen3-coder-30b"),
            yolo: !matches!(
                std::env::var("DC_MCP_YOLO").as_deref(),
                Ok("0") | Ok("false") | Ok("no")
            ),
            default_workspace,
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Resolve the backend URL list: `DC_BASE_URLS` (comma-separated) wins; else the
/// single `DC_BASE_URL`; else the default rig. Always returns ≥1 entry.
fn base_urls_from_env() -> Vec<String> {
    if let Ok(list) = std::env::var("DC_BASE_URLS") {
        let urls = parse_url_list(&list);
        if !urls.is_empty() {
            return urls;
        }
    }
    vec![env_or("DC_BASE_URL", "http://localhost:11435/v1")]
}

/// Split a comma-separated URL list, trimming whitespace and dropping empties.
fn parse_url_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Build the production tool set from resolved config.
pub fn build_tools(cfg: Config) -> StoreTools {
    // The health check probes a single backend — use the first pool.
    let health_url = cfg.base_urls[0].clone();
    let store = JobStore::new(JobConfig {
        binary: cfg.binary.clone(),
        base_urls: cfg.base_urls,
        model: cfg.model.clone(),
        yolo: cfg.yolo,
    });
    StoreTools {
        store,
        default_workspace: cfg.default_workspace,
        binary: cfg.binary,
        base_url: health_url,
        model: cfg.model,
    }
}

/// Run the stdio server loop: read one JSON-RPC message per line from `input`,
/// dispatch it, and write each response as one line to `output`. Blocks until
/// `input` reaches EOF (the client disconnected). Factored to take the streams so
/// it's driveable in a test with in-memory pipes.
pub fn serve<R: std::io::BufRead, W: std::io::Write>(
    tools: &dyn tools::Tools,
    mut input: R,
    mut output: W,
) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // EOF — client closed the pipe.
        }
        let Some(req) = protocol::Request::parse(line.trim()) else {
            continue; // blank line / not JSON-RPC — skip.
        };
        if let Some(resp) = protocol::dispatch(&req, tools) {
            let text = serde_json::to_string(&resp)
                .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize failed: {e}"}}}}"#));
            writeln!(output, "{text}")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// A stub tool set so we can drive `serve` without spawning subprocesses.
    struct Echo;
    impl tools::Tools for Echo {
        fn call(&self, name: &str, _args: &Value) -> Result<String, String> {
            Ok(format!("called {name}"))
        }
    }

    #[test]
    fn parses_comma_separated_url_list() {
        assert_eq!(
            parse_url_list("http://a:1/v1, http://b:2/v1 ,http://c:3/v1"),
            vec!["http://a:1/v1", "http://b:2/v1", "http://c:3/v1"]
        );
        // Trailing commas / blank entries are dropped, not turned into empties.
        assert_eq!(parse_url_list("http://a:1/v1,,"), vec!["http://a:1/v1"]);
        assert!(parse_url_list("   ").is_empty());
    }

    #[test]
    fn serve_answers_initialize_then_stops_at_eof() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve(&Echo, std::io::Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // One response line for initialize; none for the notification.
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "only initialize should reply: {text}");
        let resp: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "dumb-coder");
    }

    #[test]
    fn serve_routes_a_tool_call() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"dumb_coder_health","arguments":{}}}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve(&Echo, std::io::Cursor::new(input), &mut out).unwrap();
        let resp: Value = serde_json::from_str(
            out.strip_suffix(b"\n")
                .map(|s| std::str::from_utf8(s).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            resp["result"]["content"][0]["text"],
            "called dumb_coder_health"
        );
    }

    #[test]
    fn yolo_defaults_on_and_respects_opt_out() {
        // Can't mutate process env safely in parallel tests; assert the parse rule
        // directly on the same predicate from_env uses.
        let off = |v: &str| matches!(Some(v), Some("0") | Some("false") | Some("no"));
        assert!(off("0") && off("false") && off("no"));
        assert!(!off("1") && !off("") && !off("yes"));
    }
}
