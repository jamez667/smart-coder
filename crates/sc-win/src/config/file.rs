//! The machine-local `config.json`: where it lives, and the pure parse/serialize pair.
//!
//! Every field is optional — a missing, blank, or malformed file yields all-`None` and
//! each caller keeps its own default, so a bad file degrades to the compiled defaults
//! rather than failing the launch.

/// The machine-local config file: `%APPDATA%\smart-coder\config.json` on Windows,
/// falling back to the system temp dir so we always have *somewhere* to look. This is
/// the same directory convention as [`crate::persist`]'s state file — deliberately kept
/// together. It is NOT tracked by git; each box supplies its own endpoint/model here.
pub(super) fn config_file() -> std::path::PathBuf {
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
    /// How the app works: `"craft"` / `"assistant"` (spec 21). Absent ⇒ never chosen, which
    /// raises the first-run prompt. A garbage value parses back to `None` and so asks again —
    /// deliberately, because guessing a mode for someone who chose Craft would be the one
    /// failure this feature cannot afford.
    pub mode: Option<String>,
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

    // --- Endpoint-agnostic knobs (spec 21) ---
    // These used to reset on every restart because `save_config` wrote only the connection
    // fields. A setting the app forgets is one the user re-enters forever, so they are stored
    // like everything else — each still `Option`, so absent means "keep the compiled default"
    // rather than "false".
    /// The verification command (`cargo test`, `npm test`, …).
    pub verify_command: Option<String>,
    /// Permission posture. **Absent means the default (`false`), never `true`** — a config file
    /// that lost this key must not silently come back with permissions loosened.
    pub yolo: Option<bool>,
    /// Plan-only: run the pipeline without writing files.
    pub dry_run: Option<bool>,
    /// The Unity editor path override (Settings ▸ General). Blank ⇒ find it via the Hub
    /// convention, which is the common case; this is for the machine where that fails.
    pub unity_path: Option<String>,

    // --- Claude Code panel options (spec 22) ---
    /// Model alias (`opus` / `sonnet` / `haiku`); absent ⇒ the CLI's own default.
    pub claude_model: Option<String>,
    /// Permission mode (`acceptEdits` / `plan`); absent ⇒ the CLI asks as usual.
    pub claude_permission: Option<String>,
    /// Carry the previous run's context. Absent ⇒ `false`, like every other flag here.
    pub claude_continue: Option<bool>,
    /// Space-separated tool lists, stored as written so a pattern like `Bash(git *)` survives.
    pub claude_allowed_tools: Option<String>,
    pub claude_disallowed_tools: Option<String>,
    /// Extra directories, one per line (a path may contain spaces, so lines not spaces).
    pub claude_add_dirs: Option<String>,
}

/// Pull the connection fields out of the config JSON. Any key may be absent; a
/// missing/blank/malformed file yields an all-`None` [`ConfigFields`] and the caller keeps
/// its defaults. Dependency-free (serde_json, already in the tree) — mirrors `persist::parse`.
pub(super) fn parse_config(text: &str) -> ConfigFields {
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
        mode: field("mode"),
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
        verify_command: field("verify_command"),
        unity_path: field("unity_path"),
        claude_model: field("claude_model"),
        claude_permission: field("claude_permission"),
        claude_continue: v.get("claude_continue").and_then(|x| x.as_bool()),
        claude_allowed_tools: field("claude_allowed_tools"),
        claude_disallowed_tools: field("claude_disallowed_tools"),
        claude_add_dirs: field("claude_add_dirs"),
        // A NON-boolean value reads as absent rather than as `true`: a hand-edited
        // `"yolo": "yes"` must fall back to the safe default, not enable it.
        yolo: v.get("yolo").and_then(|x| x.as_bool()),
        dry_run: v.get("dry_run").and_then(|x| x.as_bool()),
    }
}

/// Serialize the connection fields to config.json text, omitting any that are unset so the
/// file stays minimal (and a blank key never lands in it). Pure/host-testable — the write
/// happens in [`UiConfig::save_config`].
pub(super) fn serialize_config(f: &ConfigFields) -> String {
    let mut obj = serde_json::Map::new();
    let mut put = |k: &str, v: &Option<String>| {
        if let Some(s) = v.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            obj.insert(k.to_string(), serde_json::Value::String(s.to_string()));
        }
    };
    put("mode", &f.mode);
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
    put("verify_command", &f.verify_command);
    put("unity_path", &f.unity_path);
    put("claude_model", &f.claude_model);
    put("claude_permission", &f.claude_permission);
    put("claude_allowed_tools", &f.claude_allowed_tools);
    put("claude_disallowed_tools", &f.claude_disallowed_tools);
    put("claude_add_dirs", &f.claude_add_dirs);
    // Written only when set, matching every other field: a `false` is the default, and writing
    // it would turn "unset" into "explicitly off" in a file people hand-edit.
    let mut put_bool = |k: &str, v: &Option<bool>| {
        if let Some(b) = v {
            obj.insert(k.to_string(), serde_json::Value::Bool(*b));
        }
    };
    put_bool("yolo", &f.yolo);
    put_bool("dry_run", &f.dry_run);
    put_bool("claude_continue", &f.claude_continue);
    serde_json::Value::Object(obj).to_string()
}

/// Whether `url` looks like Google's Gemini OpenAI-compat endpoint — used only by the migration
/// in `UiConfig::load` to classify a pre-connections config's stages. Matches on the host so a
/// trailing-slash or path variation still counts.
pub(super) fn is_gemini_url(url: &str) -> bool {
    url.contains("generativelanguage.googleapis.com")
}
