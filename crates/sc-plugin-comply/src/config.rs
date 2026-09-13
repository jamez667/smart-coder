//! Where the optional prose model lives.
//!
//! Deliberately tiny, and deliberately **not** the editor's `UiConfig`. That struct is
//! thirty-one fields of endpoints, keys, provider routing, swarm knobs and permission
//! posture; the audit reads exactly three of them. Importing it would have meant this
//! plugin depending on the agent's whole configuration surface to find a base URL.
//!
//! It is also the reason compliance is a plugin at all. The evidence engine (`sc-comply`)
//! cannot reach a model — its own crate graph forbids it — but the *prose* can, and the
//! editor's guarantee is that it keeps no path to a model. Moving both here keeps the
//! summary and the auditor guidance working while the editor sheds `sc-model` entirely.

/// Which endpoint the prose model runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Provider {
    /// The local OpenAI-compatible server (llama.cpp / Ollama). Key normally blank.
    #[default]
    Local,
    /// Gemini via its OpenAI-compatible endpoint. Carries the Gemini API key.
    Gemini,
}

/// An endpoint plus its optional key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    pub base_url: String,
    pub key: Option<String>,
}

/// The three settings the audit's prose path needs.
///
/// Loaded from this plugin's own `config.json`, beside its binary — not the host's. A
/// plugin reaching into the editor's configuration file would be reaching somewhere it
/// does not belong (spec 25), and these values mean nothing to the editor now that it
/// makes no model calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComplyConfig {
    /// The local endpoint.
    pub local_conn: Connection,
    /// The Gemini endpoint and key.
    pub gemini_conn: Connection,
    /// The model name used against [`Provider::Local`]. Gemini picks its own default,
    /// overridable with `SC_NARRATIVE_MODEL`.
    pub model: String,
}

impl ComplyConfig {
    /// The endpoint for `p`.
    pub fn connection(&self, p: Provider) -> &Connection {
        match p {
            Provider::Local => &self.local_conn,
            Provider::Gemini => &self.gemini_conn,
        }
    }

    /// Where the settings live: `config.json` beside this plugin's binary.
    fn path() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("config.json"))
    }

    /// Load, or the defaults. A missing or malformed file is the fresh-install case, not a
    /// failure — the audit runs deterministically without any of this.
    pub fn load() -> Self {
        let Some(text) = Self::path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        let field = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Self {
            local_conn: Connection {
                base_url: field("local_url").unwrap_or_default(),
                key: field("local_key"),
            },
            gemini_conn: Connection {
                base_url: field("gemini_url").unwrap_or_default(),
                key: field("gemini_key"),
            },
            model: field("model").unwrap_or_default(),
        }
    }
}
