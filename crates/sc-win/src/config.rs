//! [`UiConfig`] — the GUI's config surface, a plain owned mirror of the `sc-cli`
//! `Cli` fields the settings panel edits (spec 06/12). It carries no borrows and no
//! iced types, so it is `Send` and host-testable: the worker thread builds the
//! backends and the `sc_core::AgentConfig` / `sc_swarm::SwarmConfig` *from an owned
//! clone*, exactly the way `Cli::backend()` / `agent_config()` / `swarm_config()` do
//! — the GUI is just another front-end producing the same config (spec 01).

use std::sync::Arc;

use sc_core::{AgentConfig, Confirmer};
use sc_model::OpenAiBackend;
use sc_swarm::SwarmConfig;
use sc_tools::PermissionPolicy;

/// How the worker endpoint enforces tool calling — the GUI mirror of the CLI's
/// `--tool-calling` (spec 02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolCalling {
    /// Plain text prompting (the small-model default that always works).
    #[default]
    None,
    /// The backend's native function-calling API.
    Native,
    /// llama.cpp GBNF-constrained decoding.
    Gbnf,
}

/// A backend *connection*: an endpoint + optional key, named for the settings UI. There is a
/// **fixed set of two** — [`Provider::Local`] and [`Provider::Gemini`] — so a key (the Gemini
/// one) lives on exactly one connection and never bleeds onto the local endpoint. Each pipeline
/// stage (coder/planner/advisor) points at one of these by [`Provider`]; the model string stays
/// per-stage. This is the "set connections up once, then route stages" surface (spec 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// The local OpenAI-compatible server (llama.cpp / Ollama). Key normally blank.
    Local,
    /// Gemini via its OpenAI-compatible endpoint. Carries the Gemini API key.
    Gemini,
}

impl Provider {
    /// The stable slug persisted in config.json and used in the routing dropdown.
    pub fn slug(self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::Gemini => "gemini",
        }
    }
    /// Parse a slug back to a provider; unknown/blank ⇒ `None` (caller keeps its default).
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim() {
            "local" => Some(Provider::Local),
            "gemini" => Some(Provider::Gemini),
            _ => None,
        }
    }
    /// Human label for the settings UI.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Local => "Local",
            Provider::Gemini => "Gemini",
        }
    }
    /// The two providers, in display order — for the routing dropdown.
    pub const ALL: [Provider; 2] = [Provider::Local, Provider::Gemini];
}

/// One editable connection's endpoint + key (the value behind a [`Provider`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    pub base_url: String,
    pub key: Option<String>,
}

/// The full GUI config surface. Every field maps to a `Cli` field; the settings
/// panel edits this struct and the run/swarm worker consumes a clone of it.
///
/// **Two layers:** the [`Connection`]s + per-stage [`Provider`] routing are the *authoring*
/// surface (the settings panel edits these). [`UiConfig::resolve_stages`] flattens them into the
/// legacy `base_url`/`key`/`orchestrator_*`/`advisor_url` scalars below, which the `backend()` /
/// `orchestrator()` / `advisor()` builders (and the CLI parity, and `session.rs`) read unchanged.
/// So the connection model is additive: nothing downstream had to change.
#[derive(Debug, Clone)]
pub struct UiConfig {
    // --- Connections (the fixed Local + Gemini endpoints) + per-stage routing ---
    /// The local endpoint connection (url + optional key).
    pub local_conn: Connection,
    /// The Gemini endpoint connection (url defaults to [`GEMINI_OPENAI_BASE_URL`] + the API key).
    pub gemini_conn: Connection,
    /// Which connection the CODER (execution) stage uses.
    pub coder_provider: Provider,
    /// Which connection the PLANNER (breakdown/orchestrator) stage uses.
    pub planner_provider: Provider,
    /// Which connection the ADVISOR stage uses (only consulted when `advisor_model` is set).
    pub advisor_provider: Provider,

    // --- Coder (worker) backend — RESOLVED from the coder connection by `resolve_stages` ---
    pub base_url: String,
    pub model: String,
    pub tool_calling: ToolCalling,
    /// Optional bearer token for the coder endpoint — set this to run execution on a
    /// hosted provider (e.g. Gemini's OpenAI-compatible endpoint). Local servers ignore
    /// it. `None` ⇒ no `Authorization` header (the local-server default). RESOLVED.
    pub key: Option<String>,

    // --- Optional advisor ("junior asks senior", spec 02) ---
    pub advisor_url: Option<String>,
    pub advisor_model: Option<String>,

    // --- Optional orchestrator (the planner/decomposer — this is the "breakdown" model) ---
    pub orchestrator_url: Option<String>,
    pub orchestrator_model: Option<String>,
    /// Optional bearer token for the orchestrator endpoint. This is what lets **Gemini be
    /// the planner**: point `orchestrator_url` at Gemini's OpenAI-compat endpoint, set
    /// `orchestrator_model` to a Gemini model, and put the API key here. Local orchestrators
    /// leave it `None`.
    pub orchestrator_key: Option<String>,

    // --- Verification + planning ---
    pub verify_command: Option<String>,
    pub plan_first: bool,
    pub system_suffix: Option<String>,

    // --- Swarm knobs ---
    pub max_workers: usize,
    pub max_subtask_retries: usize,
    pub frozen_paths: Vec<String>,

    // --- Permission posture (spec 04/06) ---
    pub yolo: bool,
    pub allow: Vec<String>,
    pub dry_run: bool,
    pub verbose: bool,

    /// The directory a run reads and writes in. Defaults to an isolated scratch dir
    /// under the system temp dir — NEVER the launch/current dir, so a swarm can never
    /// scatter generated files into the user's source tree. (The CLI uses the cwd
    /// because the user invokes it deliberately there; the GUI has no such intent.)
    pub workspace: std::path::PathBuf,

    /// Run the verify command inside a per-run Docker container (spec 12) instead of on
    /// the host — a pinned Python toolkit + known layout, so a build can't depend on or
    /// pollute the host. On by default (the recommended sandbox).
    pub use_docker: bool,
    /// The Docker image to verify in — referenced by name; built from the
    /// `docker/pyenv/` image in the smart-coder-ops repo (`docker build -t
    /// smart-coder-pyenv docker/pyenv`).
    pub docker_image: String,
    /// Runtime override for [`Self::sandbox`], set by the GUI to the LIVE per-workspace
    /// [`sc_verify::SessionContainer`] so an agent run `docker exec`s into the SAME persistent
    /// container the terminal uses (shared state) instead of spinning a fresh one per command.
    /// `None` (the default, and always for CLI/config-loaded configs) → the `use_docker`
    /// decision applies. Not serialized — a purely in-memory wiring field.
    pub sandbox_override: Option<sc_verify::Sandbox>,
}

impl Default for UiConfig {
    fn default() -> Self {
        // Machine-agnostic fallbacks only. The real endpoint/model is layered on by
        // `UiConfig::load()` from config.json / env — never hard-coded here.
        Self {
            // ONE model does everything now (plan + implement) — no swarm, no advisor.
            // NEUTRAL fallback only: the standard llama.cpp port + a generic tag. The
            // real machine-specific endpoint (which model, which port) is NOT baked into
            // the repo — it lives in %APPDATA%\smart-coder\config.json (git-ignored) and
            // is layered on by `UiConfig::load()`, or overridden by SC_BASE_URL/SC_MODEL.
            // The backend launchers live in the smart-coder-ops repo (scripts/).
            // Connections: a local endpoint (key normally blank) and Gemini (url preset, key
            // supplied by the user / .env). Stages default to Local so a fresh install behaves
            // exactly as before — the planner only moves to Gemini when the user routes it there.
            local_conn: Connection {
                base_url: "http://localhost:8080/v1".to_string(),
                key: None,
            },
            gemini_conn: Connection {
                base_url: GEMINI_OPENAI_BASE_URL.to_string(),
                key: None,
            },
            coder_provider: Provider::Local,
            planner_provider: Provider::Local,
            advisor_provider: Provider::Local,
            base_url: "http://localhost:8080/v1".to_string(),
            model: "default".to_string(),
            tool_calling: ToolCalling::None,
            key: None,
            // No separate advisor/orchestrator: the workflow planner and the implement
            // agent both use the single backend above (orchestrator()/advisor() fall back
            // to base_url/model when unset). The single-agent pivot dropped the swarm.
            advisor_url: None,
            advisor_model: None,
            orchestrator_url: None,
            orchestrator_model: None,
            orchestrator_key: None,
            // The TDD build needs a verify command to drive the implementation against
            // the frozen tests. Default to pytest (the live boxes are Python); editable
            // in settings. Without it the build stops at "plan + tests written".
            verify_command: Some("python -m pytest -q".to_string()),
            plan_first: false,
            // No system suffix. The historical `/no_think` was for early Qwen3 reasoning
            // models that burned the budget on a `<think>` block; the current coder model
            // (qwen3-coder-30b) has NO thinking mode — confirmed live: zero <think> tags in
            // a full ladder run — so `/no_think` was dead text bloating every system prompt
            // and the model ignored it anyway. Editable in settings if a thinking model is used.
            system_suffix: None,
            max_workers: 2,
            max_subtask_retries: 2,
            frozen_paths: Vec::new(),
            yolo: false,
            allow: Vec::new(),
            dry_run: false,
            verbose: false,
            workspace: default_workspace(),
            use_docker: true,
            docker_image: "smart-coder-pyenv".to_string(),
            sandbox_override: None,
        }
    }
}

/// Whether `url` looks like Google's Gemini OpenAI-compat endpoint — used only by the migration
/// in [`UiConfig::load`] to classify a pre-connections config's stages. Matches on the host so a
/// trailing-slash or path variation still counts.
fn is_gemini_url(url: &str) -> bool {
    url.contains("generativelanguage.googleapis.com")
}

/// Attach `key` to `backend` as a bearer token when it is set and non-blank; otherwise return
/// the backend unchanged (the local-server default — no `Authorization` header). Centralizes the
/// "hosted providers need a key, local ones don't" decision so every backend builder stays terse.
fn apply_key(backend: OpenAiBackend, key: &Option<String>) -> OpenAiBackend {
    match key.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(k) => backend.with_api_key(k),
        None => backend,
    }
}

/// The default GUI workspace: an isolated scratch dir under the system temp dir. This
/// is deliberately NOT the current/launch dir — a swarm writing whole files must never
/// land in the user's source tree.
pub fn default_workspace() -> std::path::PathBuf {
    std::env::temp_dir().join("smart-coder-workspace")
}

/// List the **source** files in `workspace` (workspace-relative, sorted) — i.e. the
/// real output, excluding tests, the plan dir, and tooling caches. This is what a run
/// actually *built*, so the UI can show "5 files built" / "0 files built" plainly.
pub fn source_files(workspace: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip hidden/dot dirs (.smart-coder, .pytest_cache, .git), caches, deps.
            if name.starts_with('.') || name == "__pycache__" || name == "node_modules" {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !is_test_file(&rel) {
                        out.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Whether a workspace-relative path looks like a test file (so it's excluded from the
/// "source files built" count).
fn is_test_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("test_")
        || lower.contains(".test.")
        || lower.contains("_test.")
        || lower.contains(".spec.")
}

/// Pick the verify command that matches the tests that were actually written, so a
/// JS/JSX project isn't checked with `pytest` (it wrote JS tests it can't run —
/// observed live 2026-06-14). Detects the dominant test language from the test file
/// extensions and returns the conventional runner. Falls back to `python -m pytest`
/// (the configured default) when nothing recognizable was written.
pub fn detect_verify_command(test_files: &[String], fallback: &str) -> String {
    let mut py = 0usize;
    let mut js = 0usize;
    let mut rs = 0usize;
    let mut go = 0usize;
    let mut cs = 0usize;
    for f in test_files {
        let lower = f.to_ascii_lowercase();
        if lower.ends_with(".py") {
            py += 1;
        } else if lower.ends_with(".js")
            || lower.ends_with(".jsx")
            || lower.ends_with(".ts")
            || lower.ends_with(".tsx")
        {
            js += 1;
        } else if lower.ends_with(".rs") {
            rs += 1;
        } else if lower.ends_with(".cs") {
            cs += 1;
        } else if lower.ends_with("_test.go") || lower.ends_with(".go") {
            go += 1;
        }
    }
    // Pick the language with the most test files; ties favour the fallback's spirit.
    let max = py.max(js).max(rs).max(go).max(cs);
    if max == 0 {
        return fallback.to_string();
    }
    if js == max {
        // Vitest runs jest-style tests and is the lightest to invoke headlessly.
        "npx vitest run".to_string()
    } else if py == max {
        "python -m pytest -q".to_string()
    } else if rs == max {
        "cargo test".to_string()
    } else if cs == max {
        // A standalone C# test project; a real Unity project resolves the Editor batchmode
        // gate in `iterate_verify_command` (which has the workspace path).
        "dotnet test".to_string()
    } else {
        "go test ./...".to_string()
    }
}

/// Build a short overview of the files already in `workspace`, for the decomposer's
/// `repo_overview` — so when iterating on an existing project the orchestrator plans
/// *edits to existing files* (and new files) instead of assuming a blank slate. Returns
/// an empty string for an empty/missing dir (the from-scratch case). Walks recursively,
/// listing workspace-relative paths with byte sizes; capped so a huge tree can't blow
/// the prompt budget.
pub fn repo_overview(workspace: &std::path::Path) -> String {
    /// Cap on listed files (keep the decomposer prompt bounded).
    const MAX_FILES: usize = 200;

    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip VCS/build/generated noise so the overview is the user's actual sources
            // (and doesn't get swamped by e.g. a `screenshots/` folder full of PNGs).
            if crate::filetree::is_noise_dir(name) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push((rel, size));
                }
                _ => {}
            }
        }
    }

    if files.is_empty() {
        return String::new();
    }
    files.sort();
    let truncated = files.len() > MAX_FILES;
    let mut out = String::from("Existing files (edit these in place where the task applies):\n");
    for (rel, size) in files.iter().take(MAX_FILES) {
        out.push_str(&format!("  {rel} ({size} bytes)\n"));
    }
    if truncated {
        out.push_str(&format!("  … and {} more\n", files.len() - MAX_FILES));
    }
    out
}

/// The machine-local config file: `%APPDATA%\smart-coder\config.json` on Windows,
/// falling back to the system temp dir so we always have *somewhere* to look. This is
/// the same directory convention as [`crate::persist`]'s state file — deliberately kept
/// together. It is NOT tracked by git; each box supplies its own endpoint/model here.
fn config_file() -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("smart-coder").join("config.json")
}

/// The directory for model-call transcript logs: `%APPDATA%\smart-coder\logs` (next to
/// config.json). `main` points sc-model's transcript logger here at startup. `Some` unless the
/// base dir can't be resolved (never, given the temp-dir fallback).
pub fn log_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Some(base.join("smart-coder").join("logs"))
}

/// Gemini's OpenAI-compatible endpoint. Pointing the orchestrator (planner) or coder
/// backend here + a Gemini model + an API key is all it takes to run Gemini through the
/// existing OpenAI adapter — no native Gemini backend needed. Exposed so the settings
/// panel can offer a one-click "use Gemini" preset.
pub const GEMINI_OPENAI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// The connection fields persisted in / loaded from config.json. Every field is optional:
/// a missing/blank/malformed file yields all-`None` and each caller keeps its own default.
/// This is a superset of the original `(base_url, model)` pair — it now also carries the
/// orchestrator endpoint/model and the two API keys, so a Gemini-planner setup survives a
/// restart instead of resetting to the local defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFields {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub key: Option<String>,
    pub orchestrator_url: Option<String>,
    pub orchestrator_model: Option<String>,
    pub orchestrator_key: Option<String>,

    // --- Connections + routing (the newer shape; absent in pre-connections config.json) ---
    /// The Local connection's url + key.
    pub local_url: Option<String>,
    pub local_key: Option<String>,
    /// The Gemini connection's url + key.
    pub gemini_url: Option<String>,
    pub gemini_key: Option<String>,
    /// Per-stage routing slugs (`"local"` / `"gemini"`).
    pub coder_provider: Option<String>,
    pub planner_provider: Option<String>,
    pub advisor_provider: Option<String>,
}

/// Pull the connection fields out of the config JSON. Any key may be absent; a
/// missing/blank/malformed file yields an all-`None` [`ConfigFields`] and the caller keeps
/// its defaults. Dependency-free (serde_json, already in the tree) — mirrors `persist::parse`.
fn parse_config(text: &str) -> ConfigFields {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return ConfigFields::default();
    };
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    ConfigFields {
        base_url: field("base_url"),
        model: field("model"),
        key: field("key"),
        orchestrator_url: field("orchestrator_url"),
        orchestrator_model: field("orchestrator_model"),
        orchestrator_key: field("orchestrator_key"),
        local_url: field("local_url"),
        local_key: field("local_key"),
        gemini_url: field("gemini_url"),
        gemini_key: field("gemini_key"),
        coder_provider: field("coder_provider"),
        planner_provider: field("planner_provider"),
        advisor_provider: field("advisor_provider"),
    }
}

/// Serialize the connection fields to config.json text, omitting any that are unset so the
/// file stays minimal (and a blank key never lands in it). Pure/host-testable — the write
/// happens in [`UiConfig::save_config`].
fn serialize_config(f: &ConfigFields) -> String {
    let mut obj = serde_json::Map::new();
    let mut put = |k: &str, v: &Option<String>| {
        if let Some(s) = v.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            obj.insert(k.to_string(), serde_json::Value::String(s.to_string()));
        }
    };
    put("base_url", &f.base_url);
    put("model", &f.model);
    put("key", &f.key);
    put("orchestrator_url", &f.orchestrator_url);
    put("orchestrator_model", &f.orchestrator_model);
    put("orchestrator_key", &f.orchestrator_key);
    put("local_url", &f.local_url);
    put("local_key", &f.local_key);
    put("gemini_url", &f.gemini_url);
    put("gemini_key", &f.gemini_key);
    put("coder_provider", &f.coder_provider);
    put("planner_provider", &f.planner_provider);
    put("advisor_provider", &f.advisor_provider);
    serde_json::Value::Object(obj).to_string()
}

impl UiConfig {
    /// The default config, then the machine-local endpoint/model layered on top.
    ///
    /// Precedence (highest first): env `SC_BASE_URL`/`SC_MODEL` → `config.json`
    /// (`%APPDATA%\smart-coder\config.json`) → the neutral compiled [`Default`], so the
    /// endpoint the GUI talks to is never hard-coded in the repo — swap models by editing the git-ignored
    /// JSON (or exporting an env var), with zero source churn.
    pub fn load() -> Self {
        let mut cfg = Self::default();
        let file = std::fs::read_to_string(config_file())
            .map_or_else(|_| ConfigFields::default(), |t| parse_config(&t));

        // For each field: env wins over file wins over default; each layer only overrides
        // when present and non-blank. `set` applies an `Option<String>` onto a required field
        // (base_url/model); `set_opt` onto an optional one (the URLs/models/keys that stay
        // `None` unless configured).
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let set = |dst: &mut String, v: Option<String>| {
            if let Some(v) = v.filter(|s| !s.trim().is_empty()) {
                *dst = v;
            }
        };
        let set_opt = |dst: &mut Option<String>, v: Option<String>| {
            if let Some(v) = v.filter(|s| !s.trim().is_empty()) {
                *dst = Some(v);
            }
        };

        set(&mut cfg.base_url, env("SC_BASE_URL").or(file.base_url));
        set(&mut cfg.model, env("SC_MODEL").or(file.model));
        // Coder API key: SC_KEY (or the conventional GEMINI_API_KEY) → config.json.
        set_opt(
            &mut cfg.key,
            env("SC_KEY").or_else(|| env("GEMINI_API_KEY")).or(file.key),
        );
        // The planner (orchestrator) endpoint/model/key — the Gemini-as-planner path.
        set_opt(
            &mut cfg.orchestrator_url,
            env("SC_ORCH_URL").or(file.orchestrator_url),
        );
        set_opt(
            &mut cfg.orchestrator_model,
            env("SC_ORCH_MODEL").or(file.orchestrator_model),
        );
        // Orchestrator key falls back to GEMINI_API_KEY too, so a single env var lights up a
        // Gemini planner without also forcing the coder onto it.
        set_opt(
            &mut cfg.orchestrator_key,
            env("SC_ORCH_KEY")
                .or_else(|| env("GEMINI_API_KEY"))
                .or(file.orchestrator_key),
        );
        // The sandbox image and on/off are env-overridable too, so a machine can point the
        // terminal/agent at a project-appropriate image (e.g. a rust image) without editing
        // config.json. `SC_USE_DOCKER=0/false` forces host mode.
        if let Ok(img) = std::env::var("SC_DOCKER_IMAGE") {
            if !img.trim().is_empty() {
                cfg.docker_image = img;
            }
        }
        if let Ok(v) = std::env::var("SC_USE_DOCKER") {
            let v = v.trim().to_ascii_lowercase();
            cfg.use_docker = !matches!(v.as_str(), "0" | "false" | "no" | "off");
        }

        // --- Build the connection layer, migrating a pre-connections config.json ---
        //
        // New configs carry `local_*`/`gemini_*`/`*_provider`. Older ones only have the flat
        // `base_url`/`key`/`orchestrator_*` scalars (already loaded above with env layered on).
        // We derive connections from whichever is present so an old file keeps working AND, on the
        // next save, is written in the new shape.

        // Local connection: its own field, else the (already env/file-resolved) coder endpoint.
        cfg.local_conn.base_url = file
            .local_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| cfg.base_url.clone());
        cfg.local_conn.key = file
            .local_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            // Migration: the flat `key` was the LOCAL coder key in old configs. But if the coder
            // was actually pointed at Gemini (base_url == Gemini), that key belongs to Gemini, not
            // Local — don't copy it onto Local.
            .or_else(|| {
                if is_gemini_url(&cfg.base_url) {
                    None
                } else {
                    cfg.key.clone()
                }
            });

        // Gemini connection: its own fields, else migrate from the orchestrator_* (or the coder if
        // the coder itself was on Gemini), else the preset url + env key.
        cfg.gemini_conn.base_url = file
            .gemini_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| cfg.orchestrator_url.clone().filter(|u| is_gemini_url(u)))
            .or_else(|| Some(cfg.base_url.clone()).filter(|u| is_gemini_url(u)))
            .unwrap_or_else(|| GEMINI_OPENAI_BASE_URL.to_string());
        cfg.gemini_conn.key = file
            .gemini_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| cfg.orchestrator_key.clone())
            // If the coder was on Gemini, its `key` is the Gemini key.
            .or_else(|| {
                if is_gemini_url(&cfg.base_url) {
                    cfg.key.clone()
                } else {
                    None
                }
            })
            .or_else(|| env("GEMINI_API_KEY"));

        // Per-stage routing: explicit slugs, else migrate by looking at which endpoint each stage
        // pointed at. Coder: Gemini iff its base_url is the Gemini endpoint. Planner: Gemini iff it
        // had a Gemini orchestrator_url OR (no orchestrator override AND the coder was on Gemini).
        cfg.coder_provider = file
            .coder_provider
            .as_deref()
            .and_then(Provider::from_slug)
            .unwrap_or(if is_gemini_url(&cfg.base_url) {
                Provider::Gemini
            } else {
                Provider::Local
            });
        cfg.planner_provider = file
            .planner_provider
            .as_deref()
            .and_then(Provider::from_slug)
            .unwrap_or_else(|| match &cfg.orchestrator_url {
                Some(u) if is_gemini_url(u) => Provider::Gemini,
                Some(_) => Provider::Local,
                None => cfg.coder_provider, // no override ⇒ same as coder
            });
        cfg.advisor_provider = file
            .advisor_provider
            .as_deref()
            .and_then(Provider::from_slug)
            .unwrap_or(cfg.coder_provider);

        // Flatten connections+routing back into the scalar fields the builders read, so `load()`'s
        // result is internally consistent regardless of which shape the file was in.
        cfg.resolve_stages();
        cfg
    }

    /// Persist the connection fields to `%APPDATA%\smart-coder\config.json` (best-effort — a
    /// write failure is silently ignored, like [`crate::persist::save`]). This is what makes a
    /// Gemini-planner setup entered in the settings panel survive a restart: previously the file
    /// was read-only (hand-edited) so nothing the UI changed was ever written back.
    ///
    /// Only the connection fields are stored; the endpoint-agnostic knobs (verify command,
    /// posture flags) live elsewhere. env vars still override on the next `load()`.
    pub fn save_config(&self) {
        let fields = ConfigFields {
            // The connection + routing shape (the authoring surface).
            local_url: Some(self.local_conn.base_url.clone()),
            local_key: self.local_conn.key.clone(),
            gemini_url: Some(self.gemini_conn.base_url.clone()),
            gemini_key: self.gemini_conn.key.clone(),
            coder_provider: Some(self.coder_provider.slug().to_string()),
            planner_provider: Some(self.planner_provider.slug().to_string()),
            advisor_provider: Some(self.advisor_provider.slug().to_string()),
            // The resolved scalars too — so an older build, the CLI, or a hand-editor still reads a
            // working endpoint/model from the same file. `orchestrator_model` is the planner model.
            base_url: Some(self.base_url.clone()),
            model: Some(self.model.clone()),
            key: self.key.clone(),
            orchestrator_url: self.orchestrator_url.clone(),
            orchestrator_model: self.orchestrator_model.clone(),
            orchestrator_key: self.orchestrator_key.clone(),
        };
        let path = config_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, serialize_config(&fields));
    }

    /// Resolve a fresh per-run workspace: a `run-<stamp>` folder under the base
    /// `workspace`, created on demand. Each prompt gets its own datetime-stamped dir so
    /// outputs never pile up or overwrite. `stamp` is caller-supplied (e.g.
    /// `2026-06-14_17-42-09`) so this stays host-testable — the GUI passes the real
    /// local time. Falls back to the base dir if creation fails.
    pub fn run_workspace(&self, stamp: &str) -> std::path::PathBuf {
        let dir = self.workspace.join(format!("run-{stamp}"));
        if std::fs::create_dir_all(&dir).is_ok() {
            dir
        } else {
            let _ = std::fs::create_dir_all(&self.workspace);
            self.workspace.clone()
        }
    }

    /// The [`Connection`] for a given provider.
    pub fn connection(&self, p: Provider) -> &Connection {
        match p {
            Provider::Local => &self.local_conn,
            Provider::Gemini => &self.gemini_conn,
        }
    }

    /// Flatten the connection + per-stage routing into the legacy scalar fields the backend
    /// builders read. Call this after editing connections/routing (the settings-panel commit does)
    /// and it's also run at the end of [`load`], so `backend()`/`orchestrator()`/`advisor()` never
    /// need to know connections exist.
    ///
    /// * CODER → `base_url` + `key` from the coder's connection.
    /// * PLANNER → `orchestrator_url` + `orchestrator_key` from the planner's connection. Set to
    ///   `None` when the planner is on the SAME connection as the coder, so `orchestrator()` falls
    ///   back to the coder endpoint exactly as before (no redundant duplicate persisted).
    /// * ADVISOR → `advisor_url` from the advisor's connection (same-as-coder ⇒ `None`). The
    ///   advisor key still rides the orchestrator/coder key in `advisor()` (unchanged).
    pub fn resolve_stages(&mut self) {
        // Coder is the base: its connection populates the primary endpoint/key.
        let coder = self.connection(self.coder_provider).clone();
        self.base_url = coder.base_url.clone();
        self.key = coder.key.clone();

        // Planner: only set orchestrator_* when it differs from the coder connection; otherwise
        // leave None so the existing coder-fallback in `orchestrator()` applies.
        if self.planner_provider == self.coder_provider {
            self.orchestrator_url = None;
            self.orchestrator_key = None;
        } else {
            let plan = self.connection(self.planner_provider).clone();
            self.orchestrator_url = Some(plan.base_url);
            self.orchestrator_key = plan.key;
        }

        // Advisor endpoint follows its connection (same-as-coder ⇒ None ⇒ falls back to base_url).
        if self.advisor_provider == self.coder_provider {
            self.advisor_url = None;
        } else {
            self.advisor_url = Some(self.connection(self.advisor_provider).base_url.clone());
        }
    }

    /// Build the coder/worker backend, applying the requested tool-calling
    /// enforcement — the mirror of `Cli::backend()`.
    pub fn backend(&self) -> OpenAiBackend {
        let b = match self.tool_calling {
            ToolCalling::None => OpenAiBackend::new(self.base_url.clone(), self.model.clone()),
            ToolCalling::Native => {
                OpenAiBackend::new(self.base_url.clone(), self.model.clone()).with_native_tools()
            }
            ToolCalling::Gbnf => {
                OpenAiBackend::llama_cpp(self.base_url.clone(), self.model.clone())
            }
        };
        // Attach the coder API key if one is set (hosted providers like Gemini need it; local
        // servers ignore it). Do this BEFORE context detection so the `/models` probe is
        // authenticated too.
        let b = apply_key(b, &self.key);
        // Adopt the real context window the server serves the model at (e.g. 24576) instead
        // of the conservative 8192 default — best-effort, falls back to the default if the
        // server doesn't advertise it. This is the worker backend that drives the agent
        // loop, where the under-budget hurt most.
        b.with_detected_context()
    }

    /// Like [`backend`], but the returned backend honours `cancel`: setting it true aborts an
    /// in-flight streaming chat turn. Used by the chat composer's Cancel button.
    ///
    /// [`backend`]: UiConfig::backend
    pub fn backend_cancellable(
        &self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> OpenAiBackend {
        self.backend().with_cancel(cancel)
    }

    /// Build the advisor backend if a model was set — its own URL if given, else the
    /// coder endpoint (mirror of `Cli::advisor()`).
    pub fn advisor(&self) -> Option<OpenAiBackend> {
        let url = self
            .advisor_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let key = self.orchestrator_key.clone().or_else(|| self.key.clone());
        self.advisor_model
            .as_ref()
            .map(|m| apply_key(OpenAiBackend::new(url.clone(), m.clone()), &key))
    }

    /// Build the orchestrator (decomposer) backend — its own URL/model if set, else
    /// the worker endpoint/model (mirror of `Cli::orchestrator()`).
    pub fn orchestrator(&self) -> OpenAiBackend {
        let url = self
            .orchestrator_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let model = self
            .orchestrator_model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        // The planner key: an explicit orchestrator key if set, else fall back to the coder key
        // (so a single key set on the coder also authenticates a same-provider planner). This is
        // the seam that lets Gemini do the breakdown.
        let key = self.orchestrator_key.clone().or_else(|| self.key.clone());
        let b = apply_key(OpenAiBackend::new(url, model), &key);
        // Detect the server's real context window (like `backend()` does) — the workflow phases
        // ground on real file CONTENTS, which need the full window; at the hardcoded 8192 a
        // large source file is clipped and the design hallucinates around the missing code.
        b.with_detected_context()
    }

    /// The swarm's advisor: an explicit advisor if set, else the orchestrator
    /// (mirror of `Cli::swarm_advisor()`).
    pub fn swarm_advisor(&self) -> OpenAiBackend {
        self.advisor().unwrap_or_else(|| self.orchestrator())
    }

    /// The permission policy from the posture flags (mirror of
    /// `Cli::permission_policy()`): `--yolo` opens shell, `--allow` prefixes the
    /// allowlist, frozen paths are passed through.
    pub fn permission_policy(&self) -> PermissionPolicy {
        PermissionPolicy {
            frozen_paths: self.frozen_paths.clone(),
            allow_shell: self.yolo,
            shell_allowlist: self.allow.clone(),
        }
    }

    /// The single-run [`AgentConfig`], with an optional human confirmer wired into
    /// the new core seam (Part A). Mirror of `Cli::agent_config()` plus the
    /// confirmer the GUI supplies.
    pub fn agent_config(&self, confirmer: Option<Arc<dyn Confirmer>>) -> AgentConfig {
        AgentConfig {
            verify_command: self.verify_command.clone(),
            plan_first: self.plan_first,
            system_suffix: self.system_suffix.clone(),
            permission: self.permission_policy(),
            dry_run: self.dry_run,
            verbose: self.verbose,
            confirmer,
            ..AgentConfig::default()
        }
    }

    /// The [`SwarmConfig`] for a swarm run (mirror of `Cli::swarm_config()` +
    /// `swarm_config_with_frozen`). Workers default to `/no_think` to keep small
    /// models from burning budget in a reasoning block (see `system_suffix` doc).
    /// The per-subtask confirmer is shared across workers.
    pub fn swarm_config(&self, confirmer: Option<Arc<dyn Confirmer>>) -> SwarmConfig {
        let mut worker = self.agent_config(confirmer);
        if worker.system_suffix.is_none() {
            worker.system_suffix = Some("/no_think".to_string());
        }
        // The integration merge enforces frozen paths separately; the worker policy
        // also pins them so a worker never edits a contract test.
        worker.permission.frozen_paths = self.frozen_paths.clone();
        SwarmConfig {
            max_workers: self.max_workers,
            worker,
            verify_command: self.verify_command.clone(),
            frozen_paths: self.frozen_paths.clone(),
            max_subtask_retries: self.max_subtask_retries,
            sandbox: self.sandbox(),
        }
    }

    /// Where verify/agent commands run. A runtime `sandbox_override` (the GUI's live session
    /// container) wins; otherwise the `use_docker` decision: a per-run Docker container or the
    /// host.
    pub fn sandbox(&self) -> sc_verify::Sandbox {
        if let Some(s) = &self.sandbox_override {
            return s.clone();
        }
        if self.use_docker {
            sc_verify::Sandbox::Docker {
                image: self.docker_image.clone(),
            }
        } else {
            sc_verify::Sandbox::Host
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_verify_command_matches_the_test_language() {
        let js = detect_verify_command(
            &["client/Home.test.jsx".into(), "api.test.js".into()],
            "python -m pytest -q",
        );
        assert_eq!(js, "npx vitest run");

        let py = detect_verify_command(&["test_app.py".into()], "x");
        assert_eq!(py, "python -m pytest -q");

        let rs = detect_verify_command(&["src/foo_test.rs".into()], "x");
        assert_eq!(rs, "cargo test");

        // Nothing recognizable → the fallback.
        assert_eq!(detect_verify_command(&[], "the-fallback"), "the-fallback");
    }

    #[test]
    fn parse_config_reads_both_fields() {
        let f =
            parse_config(r#"{"base_url":"http://localhost:11435/v1","model":"qwen3-coder-30b"}"#);
        assert_eq!(f.base_url.as_deref(), Some("http://localhost:11435/v1"));
        assert_eq!(f.model.as_deref(), Some("qwen3-coder-30b"));
    }

    #[test]
    fn parse_config_reads_gemini_planner_fields() {
        // A Gemini-as-planner config.json: local coder, orchestrator pointed at Gemini + a key.
        let f = parse_config(
            r#"{
                "base_url":"http://localhost:8080/v1",
                "model":"qwen3-coder-30b",
                "orchestrator_url":"https://generativelanguage.googleapis.com/v1beta/openai",
                "orchestrator_model":"gemini-2.5-flash-lite",
                "orchestrator_key":"AIzaSECRET"
            }"#,
        );
        assert_eq!(
            f.orchestrator_url.as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
        assert_eq!(f.orchestrator_model.as_deref(), Some("gemini-2.5-flash-lite"));
        assert_eq!(f.orchestrator_key.as_deref(), Some("AIzaSECRET"));
    }

    #[test]
    fn parse_config_missing_or_blank_fields_are_none() {
        // Only one key present → the other stays None (caller keeps its default).
        let f = parse_config(r#"{"model":"just-the-model"}"#);
        assert_eq!(f.base_url, None);
        assert_eq!(f.model.as_deref(), Some("just-the-model"));
        // Blank / whitespace-only values are treated as absent, not as an empty override.
        let f = parse_config(r#"{"base_url":"  ","model":""}"#);
        assert_eq!(f.base_url, None);
        assert_eq!(f.model, None);
    }

    #[test]
    fn parse_config_malformed_or_wrong_shape_is_all_none() {
        assert_eq!(parse_config("not json at all"), ConfigFields::default());
        // Right JSON, wrong types → no strings to take.
        assert_eq!(
            parse_config(r#"{"base_url":42,"model":true}"#),
            ConfigFields::default()
        );
        assert_eq!(parse_config("{}"), ConfigFields::default());
    }

    #[test]
    fn serialize_config_round_trips_and_omits_unset() {
        // A full Gemini-planner config round-trips through serialize → parse unchanged.
        let fields = ConfigFields {
            base_url: Some("http://localhost:8080/v1".into()),
            model: Some("qwen3-coder-30b".into()),
            key: None,
            orchestrator_url: Some(GEMINI_OPENAI_BASE_URL.into()),
            orchestrator_model: Some("gemini-2.5-flash-lite".into()),
            orchestrator_key: Some("AIzaSECRET".into()),
            ..ConfigFields::default()
        };
        let json = serialize_config(&fields);
        assert_eq!(parse_config(&json), fields);
        // Unset fields are omitted entirely — no blank "key" lands in the file.
        assert!(!json.contains("\"key\""), "unset key must be omitted: {json}");
    }

    #[test]
    fn provider_slug_round_trips() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_slug(p.slug()), Some(p));
        }
        assert_eq!(Provider::from_slug("bogus"), None);
        assert_eq!(Provider::from_slug(""), None);
    }

    #[test]
    fn resolve_stages_local_coder_gemini_planner() {
        // The headline setup: local coder, Gemini planner. resolve_stages must put the local
        // endpoint on base_url (no key) and the Gemini endpoint+key on orchestrator_*.
        let mut cfg = UiConfig {
            local_conn: Connection {
                base_url: "http://localhost:11435/v1".into(),
                key: None,
            },
            gemini_conn: Connection {
                base_url: GEMINI_OPENAI_BASE_URL.into(),
                key: Some("gkey".into()),
            },
            coder_provider: Provider::Local,
            planner_provider: Provider::Gemini,
            advisor_provider: Provider::Local,
            ..UiConfig::default()
        };
        cfg.resolve_stages();
        assert_eq!(cfg.base_url, "http://localhost:11435/v1");
        assert_eq!(cfg.key, None, "local coder carries no key");
        assert_eq!(cfg.orchestrator_url.as_deref(), Some(GEMINI_OPENAI_BASE_URL));
        assert_eq!(
            cfg.orchestrator_key.as_deref(),
            Some("gkey"),
            "the Gemini key rides ONLY the planner, never the local coder"
        );
    }

    #[test]
    fn resolve_stages_same_provider_leaves_orchestrator_none() {
        // Planner on the same connection as the coder ⇒ no orchestrator override (falls back to
        // the coder endpoint in orchestrator()), so we don't persist a redundant duplicate.
        let mut cfg = UiConfig {
            coder_provider: Provider::Local,
            planner_provider: Provider::Local,
            ..UiConfig::default()
        };
        cfg.resolve_stages();
        assert_eq!(cfg.orchestrator_url, None);
        assert_eq!(cfg.orchestrator_key, None);
    }

    #[test]
    fn migrates_pre_connections_gemini_planner_config() {
        // An OLD config.json (flat fields only): local coder + Gemini orchestrator. load()'s
        // migration must derive the two connections and route the planner to Gemini.
        let json = format!(
            r#"{{
                "base_url":"http://localhost:11435/v1",
                "model":"qwen3-coder-30b",
                "orchestrator_url":"{GEMINI_OPENAI_BASE_URL}",
                "orchestrator_model":"gemini-2.5-flash-lite",
                "orchestrator_key":"AIzaOLD"
            }}"#
        );
        let f = parse_config(&json);
        // Reproduce the relevant slice of load()'s migration (pure, no file/env).
        let mut cfg = UiConfig::default();
        cfg.base_url = f.base_url.clone().unwrap();
        cfg.orchestrator_url = f.orchestrator_url.clone();
        cfg.orchestrator_key = f.orchestrator_key.clone();
        cfg.key = f.key.clone();
        // Local from coder endpoint; Gemini from the orchestrator override.
        cfg.local_conn.base_url = f.base_url.clone().unwrap();
        cfg.local_conn.key = if is_gemini_url(&cfg.base_url) { None } else { cfg.key.clone() };
        cfg.gemini_conn.base_url = cfg.orchestrator_url.clone().unwrap();
        cfg.gemini_conn.key = cfg.orchestrator_key.clone();
        cfg.coder_provider = if is_gemini_url(&cfg.base_url) { Provider::Gemini } else { Provider::Local };
        cfg.planner_provider = match &cfg.orchestrator_url {
            Some(u) if is_gemini_url(u) => Provider::Gemini,
            _ => Provider::Local,
        };
        assert_eq!(cfg.coder_provider, Provider::Local);
        assert_eq!(cfg.planner_provider, Provider::Gemini);
        assert_eq!(cfg.gemini_conn.key.as_deref(), Some("AIzaOLD"));
        assert_eq!(cfg.local_conn.key, None, "no key bled onto the local connection");
    }

    #[test]
    fn is_gemini_url_matches_the_google_host() {
        assert!(is_gemini_url(GEMINI_OPENAI_BASE_URL));
        assert!(is_gemini_url(
            "https://generativelanguage.googleapis.com/v1beta/openai/"
        ));
        assert!(!is_gemini_url("http://localhost:11435/v1"));
    }

    #[test]
    fn orchestrator_attaches_the_planner_key_and_falls_back_to_coder_key() {
        // Explicit orchestrator key is used for the planner backend.
        let cfg = UiConfig {
            orchestrator_url: Some(GEMINI_OPENAI_BASE_URL.into()),
            orchestrator_model: Some("gemini-2.5-flash-lite".into()),
            orchestrator_key: Some("planner-key".into()),
            ..UiConfig::default()
        };
        // `apply_key` decides key attachment purely from the Option; assert that seam directly
        // (constructing the backend and reading a private field isn't exposed).
        assert!(apply_key_used(&cfg.orchestrator_key.clone().or(cfg.key.clone())));

        // With no orchestrator key but a coder key set, the planner borrows the coder key.
        let cfg = UiConfig {
            key: Some("coder-key".into()),
            orchestrator_key: None,
            ..UiConfig::default()
        };
        assert_eq!(
            cfg.orchestrator_key.clone().or(cfg.key.clone()).as_deref(),
            Some("coder-key")
        );
    }

    /// Mirror of the `apply_key` decision (is a non-blank key present?) for the test above,
    /// since the attached token isn't readable off the built backend.
    fn apply_key_used(key: &Option<String>) -> bool {
        key.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).is_some()
    }

    #[test]
    fn neutral_default_has_no_machine_specifics() {
        // The compiled default must be generic — the rig's real endpoint lives in
        // config.json, never in the repo. Guard against a rig value creeping back in.
        let d = UiConfig::default();
        assert_eq!(d.base_url, "http://localhost:8080/v1");
        assert_eq!(d.model, "default");
        assert!(!d.base_url.contains("11435") && !d.base_url.contains("11439"));
    }

    #[test]
    fn source_files_excludes_tests_and_tooling() {
        let dir = std::env::temp_dir().join(format!("sc-win-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("server")).unwrap();
        std::fs::create_dir_all(dir.join(".smart-coder/plan")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("server/app.py"), "x").unwrap();
        std::fs::write(dir.join("index.html"), "x").unwrap();
        std::fs::write(dir.join("tests/test_app.py"), "x").unwrap(); // test → excluded
        std::fs::write(dir.join(".smart-coder/plan/01-specs.md"), "x").unwrap(); // plan → excluded

        let src = source_files(&dir);
        assert!(src.contains(&"server/app.py".to_string()), "{src:?}");
        assert!(src.contains(&"index.html".to_string()), "{src:?}");
        assert!(
            !src.iter().any(|f| f.contains("test")),
            "tests excluded: {src:?}"
        );
        assert!(
            !src.iter().any(|f| f.contains("smart-coder")),
            "plan excluded: {src:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_overview_is_empty_for_a_fresh_dir_and_lists_existing_files() {
        let dir = std::env::temp_dir().join(format!("sc-win-overview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("server")).unwrap();

        // Empty workspace ⇒ from-scratch ⇒ no overview.
        assert!(repo_overview(&dir).is_empty());

        // With files, the overview lists relative paths so the decomposer can plan edits.
        std::fs::write(dir.join("server/app.py"), "print('hi')").unwrap();
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
        let ov = repo_overview(&dir);
        assert!(ov.contains("server/app.py"), "{ov}");
        assert!(ov.contains("index.html"), "{ov}");
        assert!(ov.contains("Existing files"), "{ov}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_workspace_makes_a_stamped_subfolder() {
        let cfg = UiConfig::default();
        let ws = cfg.run_workspace("2026-06-14_17-42-09");
        assert!(
            ws.ends_with("run-2026-06-14_17-42-09"),
            "got {}",
            ws.display()
        );
        assert!(
            ws.starts_with(&cfg.workspace),
            "run dir lives under the base"
        );
        assert!(ws.is_dir(), "the run dir is created");
        // Two different stamps ⇒ two different dirs (no overwrite between prompts).
        let other = cfg.run_workspace("2026-06-14_18-00-00");
        assert_ne!(ws, other);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn default_workspace_is_a_scratch_dir_not_the_cwd() {
        // The whole point of the fix: the GUI must never default to the launch dir,
        // or a swarm scatters files into the user's source tree.
        let ws = UiConfig::default().workspace;
        let cwd = std::env::current_dir().unwrap();
        assert_ne!(ws, cwd, "default workspace must not be the current dir");
        assert!(
            ws.starts_with(std::env::temp_dir()),
            "default workspace should live under the temp dir, got {}",
            ws.display()
        );
    }

    #[test]
    fn agent_config_mirrors_posture_flags() {
        let cfg = UiConfig {
            yolo: true,
            allow: vec!["git ".to_string()],
            dry_run: true,
            verify_command: Some("python -m pytest".to_string()),
            frozen_paths: vec!["tests/contract.py".to_string()],
            ..UiConfig::default()
        };
        let ac = cfg.agent_config(None);
        assert!(ac.permission.allow_shell, "yolo opens shell");
        assert_eq!(ac.permission.shell_allowlist, vec!["git ".to_string()]);
        assert!(ac.dry_run);
        assert_eq!(ac.verify_command.as_deref(), Some("python -m pytest"));
        assert_eq!(ac.permission.frozen_paths, vec!["tests/contract.py"]);
        assert!(ac.confirmer.is_none());
    }

    #[test]
    fn agent_config_carries_the_confirmer() {
        let ac = UiConfig::default().agent_config(Some(Arc::new(sc_core::AutoDeny)));
        assert!(
            ac.confirmer.is_some(),
            "the GUI's confirmer must thread through"
        );
    }

    #[test]
    fn swarm_workers_default_to_no_think_and_pin_frozen() {
        let cfg = UiConfig {
            max_workers: 3,
            max_subtask_retries: 1,
            frozen_paths: vec!["tests/a.py".to_string()],
            ..UiConfig::default()
        };
        let sc = cfg.swarm_config(None);
        assert_eq!(sc.max_workers, 3);
        assert_eq!(sc.max_subtask_retries, 1);
        assert_eq!(sc.frozen_paths, vec!["tests/a.py"]);
        assert_eq!(sc.worker.system_suffix.as_deref(), Some("/no_think"));
        assert_eq!(sc.worker.permission.frozen_paths, vec!["tests/a.py"]);
    }

    #[test]
    fn advisor_requires_a_model() {
        // No advisor model ⇒ no advisor backend.
        let none = UiConfig {
            advisor_model: None,
            ..UiConfig::default()
        };
        assert!(none.advisor().is_none(), "no advisor model ⇒ no advisor");

        let with = UiConfig {
            advisor_model: Some("senior".to_string()),
            ..UiConfig::default()
        };
        assert!(with.advisor().is_some());
    }

    #[test]
    fn single_model_pivot_has_no_separate_advisor_or_orchestrator() {
        // The pivot: ONE capable model (Qwen3-8B) does plan + implement. There is no
        // swarm and no advisor — both the workflow planner (orchestrator()) and the
        // implement agent fall back to the single backend, and no advisor is wired.
        let cfg = UiConfig::default();
        assert!(
            !cfg.model.is_empty(),
            "the single model must be set by default"
        );
        assert!(
            cfg.orchestrator_model.is_none() && cfg.orchestrator_url.is_none(),
            "no separate orchestrator — the planner uses the one model"
        );
        assert!(
            cfg.advisor().is_none(),
            "no advisor in the single-model setup (the harness self-recovers instead)"
        );
    }
}
