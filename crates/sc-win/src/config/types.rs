//! The config data model: the two connections, the per-stage routing, and the
//! flattened [`UiConfig`] every backend builder reads.

use sc_model::OpenAiBackend;

use super::file::GEMINI_OPENAI_BASE_URL;
use super::workspace::default_workspace;

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

/// How the app works: with the agent, or without it (spec 21).
///
/// [`Mode::Craft`] is not "the app with the AI switched off" — it is the app's other half. The
/// model is absent *structurally*: no backend is constructed, no health probe is registered, and
/// nothing dials out. See [`UiConfig::craft`] for the single predicate every caller reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Editor, files, git, terminal — no model is ever contacted.
    Craft,
    /// Everything in Craft, plus the agent, chat and review gates.
    #[default]
    Assistant,
}

impl Mode {
    /// The stable slug persisted in config.json.
    pub fn slug(self) -> &'static str {
        match self {
            Mode::Craft => "craft",
            Mode::Assistant => "assistant",
        }
    }
    /// Parse a slug back to a mode. Unknown/blank ⇒ `None`, which the caller treats as
    /// "never chosen" — a corrupt value asks again rather than guessing on the user's behalf.
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim() {
            "craft" => Some(Mode::Craft),
            "assistant" => Some(Mode::Assistant),
            _ => None,
        }
    }
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
    /// How the app works (spec 21): [`Mode::Assistant`] (with the agent) or [`Mode::Craft`]
    /// (without it). `None` means **never chosen** — a fresh install, or a config.json whose
    /// stored value was corrupt. The distinction is what lets the first-run prompt ask once and
    /// never nag again; a bad value asks again rather than picking for the user.
    pub mode: Option<Mode>,

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
            // Unchosen. Not `Assistant`: "never asked" and "chose the agent" are different
            // states, and only the first should raise the first-run prompt.
            mode: None,
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

impl UiConfig {
    /// Whether the model is off (spec 21). **The one predicate for "no model".**
    ///
    /// Read this rather than matching on [`Self::mode`], so the unchosen case can never be
    /// mistaken for Craft: until the user answers, the app behaves as Assistant and the
    /// first-run prompt does the asking. Every backend builder and the health-probe
    /// subscription consult this.
    ///
    /// In a `craft-only` build this is unconditionally `true`, and that is the ENTIRE mechanism
    /// of that build. Every guard the feature needs already exists and already routes through
    /// here; pinning the predicate makes all of them fire at once, rather than introducing a
    /// second enforcement path that could drift from the runtime one.
    pub fn craft(&self) -> bool {
        cfg!(feature = "craft-only") || self.mode == Some(Mode::Craft)
    }

    /// Whether the user has ever chosen a mode. `false` ⇒ show the first-run prompt.
    ///
    /// A `craft-only` build has nothing to ask: there is no second mode to choose between, so the
    /// first-run question would be a dialog with one honest answer.
    pub fn mode_chosen(&self) -> bool {
        cfg!(feature = "craft-only") || self.mode.is_some()
    }

    /// Whether this build can switch modes at all.
    ///
    /// The one predicate for "offer the mode UI". A `craft-only` build hides the Settings toggle
    /// and never writes `mode` to `config.json` — writing it would leave a stale, unreachable
    /// setting behind that a later ordinary build would silently honour.
    pub fn mode_switchable(&self) -> bool {
        !cfg!(feature = "craft-only")
    }
}

/// Attach `key` to `backend` as a bearer token when it is set and non-blank; otherwise return
/// the backend unchanged (the local-server default — no `Authorization` header). Centralizes the
/// "hosted providers need a key, local ones don't" decision so every backend builder stays terse.
pub(super) fn apply_key(backend: OpenAiBackend, key: &Option<String>) -> OpenAiBackend {
    match key.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(k) => backend.with_api_key(k),
        None => backend,
    }
}
