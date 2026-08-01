//! The parsed-invocation types: what the user asked for, and with what config.

/// Default OpenAI-compatible endpoint: Ollama's compat server on localhost.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";
/// Default model — the project's primary small-model target (spec 00).
pub const DEFAULT_MODEL: &str = "gemma4:e4b";

/// What the user asked the CLI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Probe the backend and print the effective configuration.
    Doctor,
    /// Interactive chat REPL (the default with no subcommand).
    Chat,
    /// Run a coding task in the workspace with the live TUI (spec 06).
    Run { task: String },
    /// Run a task and serve a live web dashboard in the browser (spec 06).
    Serve { task: String },
    /// Serve the remote iterate server (for the Android client): idle until a phone
    /// POSTs a task to `/run`, then drives an in-place Iterate run over the open
    /// workspace. No task on the command line — the phone supplies it.
    Remote,
    /// Audit the workspace against a compliance framework pack and serve the
    /// evidence pack as a local dashboard (spec 13). No task — the pack decides
    /// what is evaluated. `--pack <path>` selects the framework.
    Comply { pack: Option<String> },
    /// Critique a compliance pack with the deterministic authoring lints
    /// (spec 14): `on_no_files` mistakes, unreachable patterns, self-referential
    /// detectors, controls claiming determinism they cannot have. No model is
    /// involved. `--pack <path>` selects the pack to lint.
    ComplyLint { pack: Option<String> },
    /// List the compliance framework packs shipped with the tool.
    ListPacks,
    /// Audit every shipped framework and write a static, REDACTED HTML site
    /// (spec 13). Suitable for GitHub Pages: file paths, line numbers and
    /// evidence excerpts are withheld. `--out <dir>` selects the destination.
    ComplyExport { out: Option<String> },
    /// Run the compliance drafting eval (spec 15): draft a suite of real
    /// framework controls and grade each against a hand-labelled expectation.
    /// Measures whether a model stays honest when the easy answer is to invent
    /// evidence. Repeat `--author-model` to compare models side by side.
    ComplyEval {
        /// Models to compare, as `name` or `name@base_url`.
        models: Vec<String>,
    },
    /// Run a task with the worker swarm (orchestrator + parallel workers) and
    /// serve the swarm dashboard (spec 08).
    Swarm { task: String },
    /// Run the staged planning workflow (specs→…→work decomposition) on a task,
    /// writing the plan artifacts to `specs/<slug>/` (spec 09). When
    /// `interactive` is set, halt at each phase boundary for a human
    /// approve/revise/send-back/abort decision; otherwise auto-approve every gate.
    Plan { task: String, interactive: bool },
    /// Plan, then BUILD a task via the staged decomposition engine: run the
    /// plan-only workflow to a stage breakdown, then land each scoped stage with
    /// `staged_build`, gated by a per-stage verify (default `cargo check
    /// --workspace`). Always emits the JSON-lines event stream — the headless
    /// entry point (it never uses the single-loop `run`).
    Staged { task: String },
    /// Check the specs against the code (spec 17): anchors that no longer
    /// resolve, assertions that are false, crates no spec claims. Deterministic —
    /// no model runs. With `check`, exits non-zero on a broken or stale claim,
    /// which is what makes it a CI gate.
    Trace { check: bool },
    /// The task queue (spec 19): file a request against any configured
    /// repository, draft its spec, approve or send it back.
    Queue { action: QueueAction },
    /// Re-render a recorded session from its JSON-lines log (spec 06). `session`
    /// is a session id (resolved under `.smart-coder/sessions/`) or a path to a log.
    Replay { session: String },
    /// Print usage.
    Help,
}

/// What to do with the task queue (spec 19).
///
/// The vocabulary is deliberately small and matches what the public web surface
/// can do: **file a request · watch it draft · read the spec · approve or send
/// back**. There is no action that builds — approving marks a spec `Ready` and
/// the developer picks it up in their IDE when they choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    /// File a request against a configured repository.
    ///
    /// `kind` decides what happens: bug/feature/improvement draft a spec,
    /// **feedback is kept as a note and never reaches a model**.
    File {
        text: String,
        repo: String,
        kind: sc_daemon::IntakeKind,
    },
    /// Show the queue: what needs a human first.
    List,
    /// Draft queued tasks until the queue is empty or the process is stopped.
    /// The queue is durable, so this is killable and resumable.
    Run,
    /// Approve a drafted spec. Settles it in the repository and marks the task
    /// `Ready`. **Starts nothing.**
    Approve {
        id: String,
    },
    /// Send a drafted spec back to be redrafted, with a note saying why.
    SendBack {
        id: String,
        notes: String,
    },
    /// Drop a task before it was approved.
    Discard {
        id: String,
    },
    /// Print the drafted spec for a task.
    Show {
        id: String,
    },
    /// Show kept feedback — the intake kind that never becomes a spec.
    Feedback {
        repo: Option<String>,
        all: bool,
    },
    /// Mark a piece of feedback read. It is kept, not deleted.
    AckFeedback {
        repo: String,
        id: String,
    },
    /// List the repositories this daemon serves.
    Repos,
    /// Serve this daemon's repositories: `add <name> <path>` / `forget <name>`.
    AddRepo {
        name: String,
        path: String,
    },
    ForgetRepo {
        name: String,
    },
    /// Link this daemon to a hosted server (spec 18).
    ///
    /// The daemon dials **out** to it and accepts no connections, so this is a
    /// client credential and there is no inbound surface it protects.
    Link {
        url: String,
        key: String,
    },
    /// Show which server this daemon is linked to, if any. Never prints the key.
    LinkStatus,
    /// Dial the linked server and draft whatever it hands over, until stopped.
    ///
    /// Distinct from [`Run`](QueueAction::Run), which drafts from the *local*
    /// queue and needs no server. Two commands rather than a flag, because they
    /// fail in different ways and a developer needs to know which one they are
    /// running when it stops.
    Serve,
}

/// Which tool-call enforcement to ask the backend for (spec 02). Maps onto the
/// backend variant and the strategy `sc-core` then selects from its capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallingArg {
    /// Plain completion + parse/repair (works against any server).
    None,
    /// OpenAI-style native function-calling.
    Native,
    /// llama.cpp GBNF grammar-constrained decoding.
    Gbnf,
}

/// A fully-resolved invocation: the command plus the backend config to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub command: Command,
    pub base_url: String,
    pub model: String,
    pub tool_calling: ToolCallingArg,
    /// Bearer token for the coder endpoint — set to run on a hosted provider (e.g. Gemini's
    /// OpenAI-compat endpoint). `--key`, or the `GEMINI_API_KEY` env var. Local servers ignore it.
    pub api_key: Option<String>,
    /// The project's test command for `run` (enables the TDD whole-suite gate).
    pub verify_command: Option<String>,
    /// Ask the planner to decompose the task before running (`run` only).
    pub plan_first: bool,
    /// A larger "senior" model consulted when the coder stalls — "junior asks
    /// senior" (spec 02). None = no advisor.
    pub advisor_model: Option<String>,
    /// The advisor's endpoint, when it runs on a *different* server than the coder
    /// (e.g. a swarm: coder on :11435, advisor on :11434). Defaults to `base_url`.
    pub advisor_url: Option<String>,
    /// A system-prompt suffix passed to the agent — a model-quirk hook (e.g.
    /// `/no_think` for Qwen3). Auto-set from the model name unless overridden.
    pub system_suffix: Option<String>,
    /// The orchestrator (decomposer/planner) model. Defaults to `model`. Point this at a Gemini
    /// model (with `--orchestrator-url` + `--orchestrator-key`) to run Gemini as the planner.
    pub orchestrator_model: Option<String>,
    /// The orchestrator's endpoint. Defaults to `base_url`.
    pub orchestrator_url: Option<String>,
    /// Bearer token for the orchestrator (planner) endpoint — the Gemini API key when the planner
    /// is Gemini. `--orchestrator-key`, falling back to `--key`/`GEMINI_API_KEY`.
    pub orchestrator_key: Option<String>,
    /// Max workers running at once for `swarm` (spec 08).
    pub max_workers: usize,
    /// Per-subtask retry cap for `swarm` (spec 08 — subtask retry). Default 2.
    pub max_subtask_retries: usize,
    /// Frozen contract-test paths for `swarm` (`--frozen a.py,b.py`, spec 08/11):
    /// the integration merge never overwrites these, and they drive the precise
    /// per-subtask scoped completion check. Empty = auto-detect test files in the
    /// workspace; an explicit list overrides the auto-detection.
    pub frozen_paths: Vec<String>,
    /// Run post-integration review over each subtask's integrated diff (`--review`,
    /// spec 16). **Off by default**: it is model calls a user opts into paying for.
    /// Answers *should this code stay?* after verification answered *does it work?*
    pub review: bool,
    /// What happens to a finding (`--review-action report|gate|retry`). Only a
    /// *corroborated* finding can ever gate or retry; an uncorroborated one is
    /// reported and ranked whatever this is set to.
    pub review_action: sc_swarm::ReviewAction,
    /// The severity at which a corroborated finding stops the run
    /// (`--review-gate low|medium|high`, default `high`).
    pub review_gate: sc_swarm::Severity,
    /// `plan` per-phase thinking base: `Some(false)` = think on every phase,
    /// `Some(true)` = `/no_think` every phase, `None` = the smart default (spec 09).
    pub think_base: Option<bool>,
    /// `plan` per-phase thinking overrides: `(phase-slug, suppress)` applied in
    /// order over the base, so individual steps can be flipped.
    pub think_steps: Vec<(String, bool)>,
    /// `plan` ceremony tier (spec 09 — "Scaling the ceremony"): which named set of
    /// phases stops at a human gate. `None` = no tier flag given.
    pub ceremony: Option<sc_workflow::Ceremony>,
    /// `plan` explicit gate set: a precise list of phases to gate, overriding
    /// `ceremony`. `None` = no `--gates` flag given.
    pub gates: Option<sc_workflow::PhaseSet>,
    /// Emit the event stream as JSON lines on stdout instead of the live TUI
    /// (`run --json`, spec 06 — machine-readable output).
    pub json: bool,
    /// Where to write the session log (JSON lines). `None` = the per-session
    /// default under `.smart-coder/sessions/`; `Some` overrides it (`--log`).
    pub log: Option<String>,
    /// Pre-approve all `run_command` shell calls (`--yolo`); wired into the agent's
    /// `PermissionPolicy` (spec 04/06).
    pub yolo: bool,
    /// Shell-command prefixes to auto-approve (`--allow`, repeatable); appended to
    /// the policy's allowlist.
    pub allow: Vec<String>,
    /// Plan/preview only — run read-only tools but never apply a mutation or run a
    /// command (`--dry-run`, spec 06). Threaded into `AgentConfig.dry_run`.
    pub dry_run: bool,
    /// Show the full assembled prompt each turn — what the model actually saw
    /// (`--verbose`/`-v`, spec 06). Threaded into `AgentConfig.verbose`.
    pub verbose: bool,
    /// Render the swarm to the terminal (line-oriented `SwarmEvent` stream)
    /// instead of serving the web dashboard (`--cli`, spec 06 "swarm rendering").
    /// `--json` implies this (NDJSON is itself a CLI surface).
    pub cli: bool,
    /// Loopback port for the `serve`/`swarm` web dashboard (`--port`, default 8177).
    /// Fixed (not OS-assigned) so a Tailscale `serve` tunnel can point at a stable
    /// port. Always bound on `127.0.0.1` — never `0.0.0.0`.
    pub port: u16,
    /// Serve the compliance dashboard without a URL token (`--no-token`).
    ///
    /// The server is bound to `127.0.0.1` regardless; this only removes the
    /// per-run secret from the URL, which is friction for a local read-only
    /// audit. Never defaulted on — a caller has to ask.
    pub no_token: bool,
}
