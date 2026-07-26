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
pub(super) fn serialize_config(f: &ConfigFields) -> String {
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

/// Whether `url` looks like Google's Gemini OpenAI-compat endpoint — used only by the migration
/// in `UiConfig::load` to classify a pre-connections config's stages. Matches on the host so a
/// trailing-slash or path variation still counts.
pub(super) fn is_gemini_url(url: &str) -> bool {
    url.contains("generativelanguage.googleapis.com")
}
