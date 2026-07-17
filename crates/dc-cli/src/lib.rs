//! `dumb-coder` CLI — the M0 surface (spec 06): a `doctor` check and a trivial
//! chat loop against a *real* backend.
//!
//! The interesting, testable logic lives here (arg parsing, the doctor report,
//! backend construction); [`crate::main`] is a thin I/O shell over it. This keeps
//! the binary unit-tested in the project's TDD style.
//!
//! M0 scope is deliberately small: prompt → model text → print, **no tools**. The
//! tool-driven agent loop already lives in `dc-core`; wiring it behind a `run`
//! subcommand is M1+ work.

use dc_model::{Capabilities, ModelBackend, OpenAiBackend};
use dc_proto::{DcError, Result};

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
    /// Run a task with the worker swarm (orchestrator + parallel workers) and
    /// serve the swarm dashboard (spec 08).
    Swarm { task: String },
    /// Run the staged planning workflow (specs→…→work decomposition) on a task,
    /// writing the plan artifacts to `.dumb-coder/plan/` (spec 09). When
    /// `interactive` is set, halt at each phase boundary for a human
    /// approve/revise/send-back/abort decision; otherwise auto-approve every gate.
    Plan { task: String, interactive: bool },
    /// Plan, then BUILD a task via the staged decomposition engine: run the
    /// plan-only workflow to a stage breakdown, then land each scoped stage with
    /// `staged_build`, gated by a per-stage verify (default `cargo check
    /// --workspace`). Always emits the JSON-lines event stream — this is the
    /// headless entry the MCP server drives (it never uses the single-loop `run`).
    Staged { task: String },
    /// Re-render a recorded session from its JSON-lines log (spec 06). `session`
    /// is a session id (resolved under `.dumb-coder/sessions/`) or a path to a log.
    Replay { session: String },
    /// Print usage.
    Help,
}

/// Which tool-call enforcement to ask the backend for (spec 02). Maps onto the
/// backend variant and the strategy `dc-core` then selects from its capabilities.
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
    /// The orchestrator (decomposer) model for `swarm`. Defaults to `model`.
    pub orchestrator_model: Option<String>,
    /// The orchestrator's endpoint for `swarm`. Defaults to `base_url`.
    pub orchestrator_url: Option<String>,
    /// Max workers running at once for `swarm` (spec 08).
    pub max_workers: usize,
    /// Per-subtask retry cap for `swarm` (spec 08 — subtask retry). Default 2.
    pub max_subtask_retries: usize,
    /// Frozen contract-test paths for `swarm` (`--frozen a.py,b.py`, spec 08/11):
    /// the integration merge never overwrites these, and they drive the precise
    /// per-subtask scoped completion check. Empty = auto-detect test files in the
    /// workspace; an explicit list overrides the auto-detection.
    pub frozen_paths: Vec<String>,
    /// `plan` per-phase thinking base: `Some(false)` = think on every phase,
    /// `Some(true)` = `/no_think` every phase, `None` = the smart default (spec 09).
    pub think_base: Option<bool>,
    /// `plan` per-phase thinking overrides: `(phase-slug, suppress)` applied in
    /// order over the base, so individual steps can be flipped.
    pub think_steps: Vec<(String, bool)>,
    /// `plan` ceremony tier (spec 09 — "Scaling the ceremony"): which named set of
    /// phases stops at a human gate. `None` = no tier flag given.
    pub ceremony: Option<dc_workflow::Ceremony>,
    /// `plan` explicit gate set: a precise list of phases to gate, overriding
    /// `ceremony`. `None` = no `--gates` flag given.
    pub gates: Option<dc_workflow::PhaseSet>,
    /// Emit the event stream as JSON lines on stdout instead of the live TUI
    /// (`run --json`, spec 06 — machine-readable output).
    pub json: bool,
    /// Where to write the session log (JSON lines). `None` = the per-session
    /// default under `.dumb-coder/sessions/`; `Some` overrides it (`--log`).
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
}

impl Cli {
    /// Parse argv (excluding the program name) into a [`Cli`].
    ///
    /// Grammar (M0): `[doctor|chat|help] [--base-url URL] [--model NAME]`. Flags
    /// may appear in any order; an unknown token is an error rather than silently
    /// ignored (spec 00 — fail loud).
    pub fn parse<I, S>(args: I) -> Result<Cli>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut command: Option<Command> = None;
        let mut base_url = DEFAULT_BASE_URL.to_string();
        let mut model = DEFAULT_MODEL.to_string();
        let mut tool_calling = ToolCallingArg::None;
        let mut verify_command = None;
        let mut plan_first = false;
        let mut think_base: Option<bool> = None;
        let mut think_steps: Vec<(String, bool)> = Vec::new();
        let mut ceremony: Option<dc_workflow::Ceremony> = None;
        let mut gates: Option<dc_workflow::PhaseSet> = None;
        let mut advisor_model = None;
        let mut advisor_url = None;
        let mut system_suffix: Option<String> = None;
        let mut orchestrator_model = None;
        let mut orchestrator_url = None;
        let mut max_workers = 2usize;
        let mut max_subtask_retries = 2usize;
        let mut frozen_paths: Vec<String> = Vec::new();
        let mut json = false;
        let mut log: Option<String> = None;
        let mut yolo = false;
        let mut allow: Vec<String> = Vec::new();
        let mut dry_run = false;
        let mut verbose = false;
        let mut cli_render = false;

        let mut it = args.into_iter().map(Into::into);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "doctor" if command.is_none() => command = Some(Command::Doctor),
                "chat" if command.is_none() => command = Some(Command::Chat),
                "replay" if command.is_none() => {
                    let session = it.next().ok_or_else(|| {
                        DcError::Eval(
                            "replay requires a session id or log path, e.g. \
                             `dumb-coder replay 1718000000000`"
                                .to_string(),
                        )
                    })?;
                    command = Some(Command::Replay { session });
                }
                // `run`/`serve`/`swarm`/`plan <task...>`: the rest forms the task + flags.
                "run" | "serve" | "swarm" | "plan" | "staged" if command.is_none() => {
                    let kind = arg.clone();
                    let rest: Vec<String> = it.by_ref().collect();
                    if rest.is_empty() {
                        return Err(DcError::Eval(format!(
                            "{kind} requires a task, e.g. `dumb-coder {kind} \"add a test\"`"
                        )));
                    }
                    // Pull flags back out of the collected task (so `run "x" --verify`
                    // works); simplest is to re-scan for our known flags.
                    let parsed = split_run_args(rest)?;
                    command = Some(match kind.as_str() {
                        "serve" => Command::Serve { task: parsed.task },
                        "swarm" => Command::Swarm { task: parsed.task },
                        "staged" => Command::Staged { task: parsed.task },
                        "plan" => Command::Plan {
                            task: parsed.task,
                            interactive: parsed.interactive,
                        },
                        _ => Command::Run { task: parsed.task },
                    });
                    if parsed.verify.is_some() {
                        verify_command = parsed.verify;
                    }
                    if parsed.advisor.is_some() {
                        advisor_model = parsed.advisor;
                    }
                    if parsed.advisor_url.is_some() {
                        advisor_url = parsed.advisor_url;
                    }
                    if parsed.orchestrator.is_some() {
                        orchestrator_model = parsed.orchestrator;
                    }
                    if parsed.orchestrator_url.is_some() {
                        orchestrator_url = parsed.orchestrator_url;
                    }
                    if let Some(n) = parsed.max_workers {
                        max_workers = n;
                    }
                    if let Some(n) = parsed.max_subtask_retries {
                        max_subtask_retries = n;
                    }
                    if let Some(f) = parsed.frozen_paths {
                        frozen_paths = f;
                    }
                    if let Some(u) = parsed.base_url {
                        base_url = u;
                    }
                    if let Some(m) = parsed.model {
                        model = m;
                    }
                    if let Some(tc) = parsed.tool_calling {
                        tool_calling = tc;
                    }
                    if parsed.no_think {
                        system_suffix = Some("/no_think".to_string());
                    }
                    think_base = parsed.think_base;
                    think_steps = parsed.think_steps;
                    ceremony = parsed.ceremony;
                    gates = parsed.gates;
                    if parsed.json {
                        json = true;
                    }
                    if parsed.log.is_some() {
                        log = parsed.log;
                    }
                    if parsed.yolo {
                        yolo = true;
                    }
                    if !parsed.allow.is_empty() {
                        allow.extend(parsed.allow);
                    }
                    if parsed.dry_run {
                        dry_run = true;
                    }
                    if parsed.verbose {
                        verbose = true;
                    }
                    if parsed.cli {
                        cli_render = true;
                    }
                    plan_first = parsed.plan || plan_first;
                }
                "help" | "--help" | "-h" => command = Some(Command::Help),
                "--verify" => {
                    verify_command = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--verify requires a command argument".to_string())
                    })?);
                }
                "--advisor" => {
                    advisor_model = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--advisor requires a model name".to_string())
                    })?);
                }
                "--advisor-url" => {
                    advisor_url = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--advisor-url requires a URL".to_string())
                    })?);
                }
                "--orchestrator" => {
                    orchestrator_model = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--orchestrator requires a model name".to_string())
                    })?);
                }
                "--orchestrator-url" => {
                    orchestrator_url = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--orchestrator-url requires a URL".to_string())
                    })?);
                }
                "--max-workers" => {
                    max_workers = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| {
                            DcError::Eval("--max-workers requires a positive integer".to_string())
                        })?;
                }
                "--max-retries" => {
                    max_subtask_retries =
                        it.next().and_then(|v| v.parse().ok()).ok_or_else(|| {
                            DcError::Eval(
                                "--max-retries requires a non-negative integer".to_string(),
                            )
                        })?;
                }
                "--frozen" => {
                    let list = it.next().ok_or_else(|| {
                        DcError::Eval("--frozen requires a comma-separated path list".to_string())
                    })?;
                    frozen_paths = parse_frozen_list(&list);
                }
                "--no-think" => system_suffix = Some("/no_think".to_string()),
                "--json" => json = true,
                "--log" => {
                    log = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--log requires a path argument".to_string())
                    })?);
                }
                "--yolo" => yolo = true,
                "--allow" => {
                    allow.push(it.next().ok_or_else(|| {
                        DcError::Eval("--allow requires a command prefix".to_string())
                    })?);
                }
                "--dry-run" => dry_run = true,
                "--verbose" | "-v" => verbose = true,
                "--cli" => cli_render = true,
                "--plan" => plan_first = true,
                "--base-url" => {
                    base_url = it.next().ok_or_else(|| {
                        DcError::Eval("--base-url requires a URL argument".to_string())
                    })?;
                }
                "--model" => {
                    model = it.next().ok_or_else(|| {
                        DcError::Eval("--model requires a NAME argument".to_string())
                    })?;
                }
                "--tool-calling" => {
                    let v = it.next().ok_or_else(|| {
                        DcError::Eval("--tool-calling requires none|native|gbnf".to_string())
                    })?;
                    tool_calling = match v.as_str() {
                        "none" => ToolCallingArg::None,
                        "native" => ToolCallingArg::Native,
                        "gbnf" => ToolCallingArg::Gbnf,
                        other => {
                            return Err(DcError::Eval(format!(
                                "--tool-calling must be none|native|gbnf, got {other:?}"
                            )))
                        }
                    };
                }
                other => {
                    return Err(DcError::Eval(format!(
                        "unknown argument: {other:?} (try `dumb-coder help`)"
                    )));
                }
            }
        }

        // No auto `/no_think`. Early Qwen3 reasoning models needed it to avoid burning the
        // budget on a `<think>` block, but the current coder model (qwen3-coder-30b) has no
        // thinking mode (confirmed live: zero <think> tags) — so it was dead prompt text the
        // model ignored. Pass `--no-think` explicitly if you run a thinking model that needs it.

        Ok(Cli {
            command: command.unwrap_or(Command::Chat),
            base_url,
            model,
            tool_calling,
            verify_command,
            plan_first,
            advisor_model,
            advisor_url,
            system_suffix,
            orchestrator_model,
            orchestrator_url,
            max_workers,
            max_subtask_retries,
            frozen_paths,
            think_base,
            think_steps,
            ceremony,
            gates,
            json,
            log,
            yolo,
            allow,
            dry_run,
            verbose,
            cli: cli_render,
        })
    }

    /// Build the per-phase thinking policy for `plan` (spec 09) from the flags:
    /// start from `--think-all`/`--no-think-all` (or the smart default), then apply
    /// each `--think <phase>` / `--nothink <phase>` override.
    pub fn think_policy(&self) -> dc_workflow::ThinkPolicy {
        use dc_workflow::{Phase, ThinkPolicy};
        let mut policy = match self.think_base {
            Some(false) => ThinkPolicy::always_think(),
            Some(true) => ThinkPolicy::never_think(),
            None => ThinkPolicy::default(),
        };
        for (slug, suppress) in &self.think_steps {
            if let Some(phase) = Phase::ALL.iter().copied().find(|p| p.slug() == slug) {
                policy = policy.with(phase, *suppress);
            }
        }
        policy
    }

    /// Resolve the set of phases that stop at a human gate for `plan` (spec 09 —
    /// "Scaling the ceremony"):
    /// 1. an explicit `--gates` list wins (precise control);
    /// 2. else the `--ceremony` tier's set;
    /// 3. else `Full` — so bare `--interactive` still gates every phase (the
    ///    behavior before adaptive ceremony existed).
    pub fn ceremony_gates(&self) -> dc_workflow::PhaseSet {
        if let Some(gates) = self.gates {
            gates
        } else {
            self.ceremony.unwrap_or(dc_workflow::Ceremony::Full).gates()
        }
    }

    /// Whether `plan` should put a human at the gates at all. A run is gated if the
    /// user asked for `--interactive`/`--gate` *or* named any ceremony policy
    /// (`--ceremony`/`--gates`) — naming a policy implies wanting the gates.
    pub fn plan_is_gated(&self, interactive: bool) -> bool {
        interactive || self.ceremony.is_some() || self.gates.is_some()
    }

    /// Build the orchestrator (decomposer) backend for `swarm`: its own
    /// `--orchestrator-url`/`--orchestrator` if set, else the worker endpoint/model.
    pub fn orchestrator(&self) -> OpenAiBackend {
        let url = self
            .orchestrator_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        let model = self
            .orchestrator_model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        OpenAiBackend::new(url, model)
    }

    /// Build the advisor (senior) backend, if `--advisor` was given — on its own
    /// `--advisor-url` if set, else the coder's endpoint ("junior asks senior",
    /// spec 02; a different *server* lets the swarm run both co-resident).
    pub fn advisor(&self) -> Option<OpenAiBackend> {
        let url = self
            .advisor_url
            .clone()
            .unwrap_or_else(|| self.base_url.clone());
        self.advisor_model
            .as_ref()
            .map(|m| OpenAiBackend::new(url.clone(), m.clone()))
    }

    /// Build the configured backend, applying the requested enforcement (spec 02).
    pub fn backend(&self) -> OpenAiBackend {
        let b = match self.tool_calling {
            ToolCallingArg::None => OpenAiBackend::new(self.base_url.clone(), self.model.clone()),
            ToolCallingArg::Native => {
                OpenAiBackend::new(self.base_url.clone(), self.model.clone()).with_native_tools()
            }
            ToolCallingArg::Gbnf => {
                OpenAiBackend::llama_cpp(self.base_url.clone(), self.model.clone())
            }
        };
        // Adopt the real context window the server serves the model at (e.g. 12288/slot)
        // instead of the conservative 8192 default — best-effort, falls back to the default
        // if the server doesn't advertise it. Mirrors `dc_win::config::backend()`; without
        // it the prompt budget is squeezed to 5120 even on a pool served at -c 36864.
        b.with_detected_context()
    }
}

/// Flags peeled out of the args that follow `run`/`serve` (which greedily
/// consume the rest of argv).
struct RunArgs {
    task: String,
    verify: Option<String>,
    advisor: Option<String>,
    advisor_url: Option<String>,
    orchestrator: Option<String>,
    orchestrator_url: Option<String>,
    max_workers: Option<usize>,
    /// `--max-retries N` — per-subtask retry cap for `swarm` (spec 08).
    max_subtask_retries: Option<usize>,
    /// `--frozen a.py,b.py` — frozen contract-test paths for `swarm` (spec 08/11).
    frozen_paths: Option<Vec<String>>,
    no_think: bool,
    plan: bool,
    /// Halt at each `plan` phase boundary for a human checkpoint (spec 09).
    interactive: bool,
    /// Per-phase thinking overrides for `plan` (spec 09): `--think-all` /
    /// `--no-think-all` set a base; `--think <phase>` / `--nothink <phase>` flip a
    /// single step. Applied in order over the default policy.
    think_base: Option<bool>, // Some(false)=think all, Some(true)=no_think all
    think_steps: Vec<(String, bool)>, // (phase-slug, suppress)
    /// `plan` ceremony tier (spec 09): `--ceremony minimal|standard|full`.
    ceremony: Option<dc_workflow::Ceremony>,
    /// `plan` explicit gate set: `--gates specs,architecture,…` (overrides tier).
    gates: Option<dc_workflow::PhaseSet>,
    // Global flags may also follow the task; capture them so they aren't swept
    // into the task string.
    base_url: Option<String>,
    model: Option<String>,
    tool_calling: Option<ToolCallingArg>,
    /// `--json` — emit the event stream as JSON lines instead of the TUI.
    json: bool,
    /// `--log <path>` — override the session-log destination.
    log: Option<String>,
    /// `--yolo` — pre-approve all shell commands.
    yolo: bool,
    /// `--allow <prefix>` (repeatable) — shell-command prefixes to auto-approve.
    allow: Vec<String>,
    /// `--dry-run` — preview only; never apply a mutation or run a command.
    dry_run: bool,
    /// `--verbose`/`-v` — show the full assembled prompt each turn.
    verbose: bool,
    /// `--cli` — render the swarm to the terminal instead of the web dashboard.
    cli: bool,
}

/// Split the args collected after `run`/`serve` into the task plus its trailing
/// `--verify X` / `--advisor M` / `--plan` flags. The task is everything else.
fn split_run_args(args: Vec<String>) -> Result<RunArgs> {
    let mut task_words = Vec::new();
    let mut verify = None;
    let mut advisor = None;
    let mut advisor_url = None;
    let mut orchestrator = None;
    let mut orchestrator_url = None;
    let mut max_workers = None;
    let mut max_subtask_retries = None;
    let mut frozen_paths = None;
    let mut no_think = false;
    let mut plan = false;
    let mut interactive = false;
    let mut think_base: Option<bool> = None;
    let mut think_steps: Vec<(String, bool)> = Vec::new();
    let mut ceremony: Option<dc_workflow::Ceremony> = None;
    let mut gates: Option<dc_workflow::PhaseSet> = None;
    let mut base_url = None;
    let mut model = None;
    let mut tool_calling = None;
    let mut json = false;
    let mut log = None;
    let mut yolo = false;
    let mut allow: Vec<String> = Vec::new();
    let mut dry_run = false;
    let mut verbose = false;
    let mut cli = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let need = |it: &mut std::vec::IntoIter<String>, flag: &str| {
            it.next()
                .ok_or_else(|| DcError::Eval(format!("{flag} requires an argument")))
        };
        match a.as_str() {
            "--verify" => verify = Some(need(&mut it, "--verify")?),
            "--advisor" => advisor = Some(need(&mut it, "--advisor")?),
            "--advisor-url" => advisor_url = Some(need(&mut it, "--advisor-url")?),
            "--orchestrator" => orchestrator = Some(need(&mut it, "--orchestrator")?),
            "--orchestrator-url" => orchestrator_url = Some(need(&mut it, "--orchestrator-url")?),
            "--max-workers" => {
                max_workers = Some(
                    need(&mut it, "--max-workers")?
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| {
                            DcError::Eval("--max-workers requires a positive integer".to_string())
                        })?,
                );
            }
            "--max-retries" => {
                max_subtask_retries =
                    Some(need(&mut it, "--max-retries")?.parse().map_err(|_| {
                        DcError::Eval("--max-retries requires a non-negative integer".to_string())
                    })?);
            }
            "--frozen" => frozen_paths = Some(parse_frozen_list(&need(&mut it, "--frozen")?)),
            "--base-url" => base_url = Some(need(&mut it, "--base-url")?),
            "--model" => model = Some(need(&mut it, "--model")?),
            "--tool-calling" => {
                let v = need(&mut it, "--tool-calling")?;
                tool_calling = Some(match v.as_str() {
                    "none" => ToolCallingArg::None,
                    "native" => ToolCallingArg::Native,
                    "gbnf" => ToolCallingArg::Gbnf,
                    other => {
                        return Err(DcError::Eval(format!(
                            "--tool-calling must be none|native|gbnf, got {other:?}"
                        )))
                    }
                });
            }
            "--no-think" => no_think = true,
            "--json" => json = true,
            "--log" => log = Some(need(&mut it, "--log")?),
            "--yolo" => yolo = true,
            "--allow" => allow.push(need(&mut it, "--allow")?),
            "--dry-run" => dry_run = true,
            "--verbose" | "-v" => verbose = true,
            "--cli" => cli = true,
            "--interactive" | "--gate" => interactive = true,
            "--think-all" => think_base = Some(false),
            "--no-think-all" => think_base = Some(true),
            "--think" => think_steps.push((need(&mut it, "--think")?, false)),
            "--nothink" => think_steps.push((need(&mut it, "--nothink")?, true)),
            "--ceremony" => {
                let tier = need(&mut it, "--ceremony")?;
                ceremony = Some(dc_workflow::Ceremony::parse(&tier).ok_or_else(|| {
                    DcError::Eval(format!(
                        "--ceremony must be minimal|standard|full, got {tier:?}"
                    ))
                })?);
            }
            "--gates" => {
                let list = need(&mut it, "--gates")?;
                gates = Some(parse_gate_set(&list)?);
            }
            "--plan" => plan = true,
            _ => task_words.push(a),
        }
    }
    Ok(RunArgs {
        task: task_words.join(" "),
        verify,
        advisor,
        advisor_url,
        orchestrator,
        orchestrator_url,
        max_workers,
        max_subtask_retries,
        frozen_paths,
        no_think,
        plan,
        interactive,
        think_base,
        think_steps,
        ceremony,
        gates,
        base_url,
        model,
        tool_calling,
        json,
        log,
        yolo,
        allow,
        dry_run,
        verbose,
        cli,
    })
}

/// Parse a `--gates` value: a comma-separated list of phase slugs into a
/// [`dc_workflow::PhaseSet`]. An unknown slug is an error (fail loud, spec 00).
fn parse_gate_set(list: &str) -> Result<dc_workflow::PhaseSet> {
    let mut phases = Vec::new();
    for raw in list.split(',') {
        let slug = raw.trim();
        if slug.is_empty() {
            continue;
        }
        let phase = dc_workflow::Phase::from_slug(slug).ok_or_else(|| {
            DcError::Eval(format!(
                "--gates: unknown phase {slug:?} (expected one of: {})",
                dc_workflow::Phase::ALL
                    .iter()
                    .map(|p| p.slug())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        phases.push(phase);
    }
    Ok(dc_workflow::PhaseSet::of(phases))
}

/// Parse a `--frozen a.py,b.py` list into trimmed, non-empty, slash-normalized
/// paths (so `tests\a.py` and `tests/a.py` compare equal downstream, matching
/// `dc_swarm`'s `is_frozen`).
fn parse_frozen_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| s.trim().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Looks like a test file by the usual Python/pytest convention: `test_*.py`,
/// `*_test.py`, or anything under a `tests/` directory. Used to auto-freeze the
/// test oracle for a free-text `swarm <task>` run when `--frozen` wasn't given.
fn looks_like_test_file(rel: &str) -> bool {
    let norm = rel.replace('\\', "/");
    let name = norm.rsplit('/').next().unwrap_or(&norm);
    let is_py = name.ends_with(".py");
    let by_name = is_py && (name.starts_with("test_") || name.ends_with("_test.py"));
    let by_dir = norm.split('/').any(|seg| seg == "tests" || seg == "test");
    by_name || (by_dir && is_py)
}

/// Auto-detect the workspace's test files (one directory level deep plus a top-level
/// `tests/`), so a free-text `swarm` run gets the precise per-subtask scoped check
/// and test-oracle protection without the user listing files by hand (spec 08/11).
/// Best-effort: an unreadable workspace yields an empty list (the swarm then falls
/// back to the whole-suite-delta check, as before).
pub fn detect_test_files(workspace: &std::path::Path) -> Vec<String> {
    fn scan(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Recurse one level (and into any `tests/` dir) — deep trees are rare
                // for the small tasks the swarm targets, and we avoid walking the world.
                let name = entry.file_name();
                let is_tests = name.to_str() == Some("tests") || name.to_str() == Some("test");
                if depth == 0 || is_tests {
                    scan(&path, base, out, depth + 1);
                }
            } else if let Ok(rel) = path.strip_prefix(base) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if looks_like_test_file(&rel) && !out.contains(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    let mut out = Vec::new();
    scan(workspace, workspace, &mut out, 0);
    out.sort();
    out
}

/// Resolve where a run's session log is written (spec 06). An explicit `--log`
/// path wins; otherwise default to `<workspace>/.dumb-coder/sessions/<id>.jsonl`,
/// where `<id>` is a millisecond timestamp — sortable, unique enough for one
/// user, and std-only (no extra crate). Returns the path and its session id.
pub fn session_log_path(
    workspace: &std::path::Path,
    log_override: Option<&str>,
) -> (std::path::PathBuf, String) {
    if let Some(p) = log_override {
        let path = std::path::PathBuf::from(p);
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        return (path, id);
    }
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "session".to_string());
    let path = sessions_dir(workspace).join(format!("{id}.jsonl"));
    (path, id)
}

/// Where session logs live: `<workspace>/.dumb-coder/sessions/` — alongside the
/// planning workflow's `.dumb-coder/plan/` (the dir is already gitignored).
pub fn sessions_dir(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join(".dumb-coder").join("sessions")
}

/// Resolve a `replay` argument to a log file (spec 06): a path is used as-is; a
/// bare id resolves to `<workspace>/.dumb-coder/sessions/<id>.jsonl`.
pub fn resolve_replay_path(workspace: &std::path::Path, session: &str) -> std::path::PathBuf {
    let direct = std::path::Path::new(session);
    if direct.is_file() {
        return direct.to_path_buf();
    }
    // A bare id (with or without the .jsonl suffix).
    let id = session.strip_suffix(".jsonl").unwrap_or(session);
    sessions_dir(workspace).join(format!("{id}.jsonl"))
}

/// Usage text (spec 06 — invocation modes, trimmed to the M0 surface).
pub fn usage() -> &'static str {
    "\
dumb-coder — an agentic coding tool for small models (M0)

USAGE:
    dumb-coder [COMMAND] [OPTIONS]

COMMANDS:
    chat            Interactive chat with the model (default)
    run <task>      Run a coding task in the current dir with a live TUI
    serve <task>    Run a task and watch it in your browser (web dashboard)
    swarm <task>    Decompose + run with parallel workers (swarm dashboard)
    plan <task>     Staged planning workflow → .dumb-coder/plan/ (spec 09)
    staged <task>   Plan + BUILD via the staged decomposition engine (JSON stream)
    replay <id>     Re-render a recorded session from its log (spec 06)
    doctor          Check the backend is reachable; print effective config
    help            Show this message

OPTIONS:
    --base-url URL        OpenAI-compatible endpoint  [default: http://localhost:11434/v1]
    --model NAME          Model to use                [default: gemma4:e4b]
    --tool-calling MODE   none | native | gbnf — how the backend enforces tool
                          calls (spec 02)             [default: none]
    --verify CMD          Test command for `run` (enables the TDD whole-suite gate)
    --advisor MODEL       A larger model consulted when the coder stalls
                          (\"junior asks senior\", spec 02).
    --advisor-url URL     Endpoint for the advisor when it runs on a different
                          server than the coder (a swarm). [default: --base-url]
    --no-think            Append /no_think to the prompt (needed for Qwen3 models;
                          auto-applied when the model name contains 'qwen3').
    --plan                Decompose the task into a plan before running (`run`)
  run output, logging & safety (spec 06):
    --json                Emit the event stream as JSON lines on stdout (no TUI)
    --log PATH            Write the session log here  [default:
                          .dumb-coder/sessions/<id>.jsonl]
    --dry-run             Preview only: run read-only tools but never apply an edit
                          or run a command; the workspace is left untouched
    --verbose, -v         Show the full assembled prompt each turn (what the model
                          actually saw); full text in --json / the session log
    --yolo                Pre-approve all run_command shell calls
    --allow PREFIX        Auto-approve shell commands starting with PREFIX
                          (repeatable, e.g. --allow \"cargo test\")
  swarm / plan (workers use --base-url/--model):
    --cli                 Render the swarm to the terminal (task board · workers ·
                          integration) instead of serving the web dashboard. `--json`
                          implies this and emits one NDJSON SwarmEvent per line.
    --orchestrator MODEL  The model that decomposes/plans         [default: --model]
    --orchestrator-url U  Endpoint for the orchestrator           [default: --base-url]
    --max-workers N       Max parallel workers                    [default: 2]
    --interactive, --gate Halt at each `plan` phase boundary for a human checkpoint:
                          approve / revise / send-back / abort (spec 09). Default is
                          autonomous (auto-approve every gate).
    --ceremony TIER       Scale the ceremony to the task (spec 09): which phases stop
                          at a gate. minimal (final sign-off only) | standard (specs,
                          tests, decomposition) | full (every phase). Implies
                          --interactive.
    --gates PHASES        Precise gate set: a comma-separated list of phase slugs to
                          gate (e.g. specs,stage-breakdown). Overrides --ceremony;
                          implies --interactive.
  plan only — per-phase thinking (spec 09; default: think on the JSON phases,
  /no_think on the prose phases):
    --think-all           Think on every phase
    --no-think-all        /no_think on every phase
    --think PHASE         Force thinking on one phase (slug, e.g. layout)
    --nothink PHASE       Force /no_think on one phase

EXAMPLES:
    dumb-coder doctor
    dumb-coder run \"make the failing test in is_even pass\" --verify \"sh test.sh\"
    dumb-coder run \"fix parse_config\" --json --verify \"cargo test\" > run.jsonl
    dumb-coder run \"refactor the parser\" --dry-run
    dumb-coder replay 1718000000000
    dumb-coder serve \"fix the bug in parse_config\" --verify \"cargo test\"
    dumb-coder swarm \"add validation and a test\" --cli --verify \"python -m pytest -q\" \\
        --base-url http://localhost:11435/v1 --model coder-0 --max-workers 2 \\
        --orchestrator-url http://localhost:11434/v1 --orchestrator advisor-e4b
    dumb-coder --model gemma4:e4b --tool-calling native"
}

impl Cli {
    /// Build the agent config from the parsed flags (used by `run`).
    pub fn agent_config(&self) -> dc_core::AgentConfig {
        dc_core::AgentConfig {
            verify_command: self.verify_command.clone(),
            plan_first: self.plan_first,
            system_suffix: self.system_suffix.clone(),
            permission: self.permission_policy(),
            dry_run: self.dry_run,
            verbose: self.verbose,
            ..Default::default()
        }
    }

    /// The permission policy from the safety flags (spec 04/06): `--yolo`
    /// pre-approves all shell, `--allow <prefix>` extends the allowlist. Frozen
    /// paths stay empty here — the swarm sets those separately.
    pub fn permission_policy(&self) -> dc_tools::PermissionPolicy {
        dc_tools::PermissionPolicy {
            allow_shell: self.yolo,
            shell_allowlist: self.allow.clone(),
            ..Default::default()
        }
    }

    /// Build the swarm config from the parsed flags (used by `swarm`). Workers run
    /// the per-worker agent config; the verify command also gates integration.
    ///
    /// Swarm workers are tiny, reasoning-prone models (Qwen3-1.7B and the like).
    /// Their `/no_think` suffix can't rely on the model-name auto-detect — a swarm
    /// run usually aliases the worker (`coder-0`) so the name never contains
    /// "qwen3". Default the worker suffix to `/no_think` unless one is set.
    pub fn swarm_config(&self) -> dc_swarm::SwarmConfig {
        let mut worker = self.agent_config();
        if worker.system_suffix.is_none() {
            worker.system_suffix = Some("/no_think".to_string());
        }
        dc_swarm::SwarmConfig {
            max_workers: self.max_workers,
            worker,
            verify_command: self.verify_command.clone(),
            // The explicit `--frozen` list, if given. When empty, the caller
            // (`main`) auto-detects test files from the workspace — done there
            // because it needs filesystem access this `&self` method lacks.
            frozen_paths: self.frozen_paths.clone(),
            max_subtask_retries: self.max_subtask_retries,
            // The CLI runs on the host (the user controls their own environment); the
            // GUI defaults to the Docker sandbox.
            sandbox: dc_swarm::Sandbox::Host,
        }
    }

    /// The advisor swarm workers consult when they stall ("junior asks senior").
    /// Prefer an explicit `--advisor`; otherwise fall back to the orchestrator —
    /// the bigger, smarter model is already in VRAM, so workers should be able to
    /// ask it for help even when no separate advisor was named.
    pub fn swarm_advisor(&self) -> OpenAiBackend {
        self.advisor().unwrap_or_else(|| self.orchestrator())
    }
}

/// Render the `doctor` report. `reachable` carries the probe result so the
/// formatting is testable without a live server.
pub fn doctor_report(cli: &Cli, caps: &Capabilities, reachable: &Result<()>) -> String {
    let status = match reachable {
        Ok(()) => "reachable ✓".to_string(),
        Err(e) => format!("UNREACHABLE ✗ — {e}"),
    };
    format!(
        "dumb-coder doctor\n\
         \x20 backend:        openai-compat\n\
         \x20 base url:       {}\n\
         \x20 model:          {}\n\
         \x20 status:         {}\n\
         \x20 context budget: {} tokens\n\
         \x20 tool calling:   {}",
        cli.base_url,
        cli.model,
        status,
        caps.max_context_tokens,
        tool_calling_word(caps.tool_calling),
    )
}

fn tool_calling_word(tc: dc_model::ToolCalling) -> &'static str {
    match tc {
        dc_model::ToolCalling::None => "parse+repair (no enforcement)",
        dc_model::ToolCalling::OpenAiStyle => "native function-calling",
        dc_model::ToolCalling::Gbnf => "GBNF grammar-constrained",
    }
}

/// Probe the backend with a tiny generation to confirm it's actually serving the
/// model — not just that the port is open (spec 06: "model is pulled").
pub fn probe(backend: &dyn ModelBackend) -> Result<()> {
    use dc_model::{GenerateRequest, Message};
    let req = GenerateRequest::new(vec![Message::user("ping")]);
    backend.generate(&req).map(|_| ())
}

/// Preflight every backend a run needs, by `(label, backend)`. Returns a clear
/// error naming the first unreachable one, so a dead/crashed server fails fast at
/// the start with a useful message instead of producing empty artifacts mid-run.
/// Backends sharing a `name()` (same model+endpoint) are only probed once.
pub fn preflight(backends: &[(&str, &dyn ModelBackend)]) -> Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for (label, backend) in backends {
        // On-device backends have no endpoint to probe.
        if backend.capabilities().on_device {
            continue;
        }
        let key = backend.name().to_string();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Err(e) = probe(*backend) {
            return Err(DcError::Eval(format!(
                "preflight: the {label} backend ({}) isn't responding ({e}). \
                 Is the server up and the model loaded?",
                backend.name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_names_the_unreachable_backend() {
        use dc_model::MockBackend;
        // An exhausted mock errors on generate → stands in for a down server.
        let down = MockBackend::new(Vec::<String>::new());
        let err = preflight(&[("orchestrator", &down)]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("orchestrator"), "{msg}");
        assert!(msg.contains("isn't responding"), "{msg}");
    }

    #[test]
    fn preflight_passes_when_reachable() {
        use dc_model::MockBackend;
        // Two pings for the single distinct backend (probe consumes one).
        let a = MockBackend::new(["pong", "pong"]);
        assert!(preflight(&[("orchestrator", &a)]).is_ok());
    }

    #[test]
    fn preflight_probes_a_shared_endpoint_once() {
        use dc_model::MockBackend;
        // Orchestrator and advisor are the SAME model (e.g. advisor-e4b on one
        // server): one scripted ping is enough because the duplicate is skipped.
        let shared = MockBackend::new(["pong"]);
        assert!(preflight(&[("orchestrator", &shared), ("advisor", &shared)]).is_ok());
    }

    #[test]
    fn defaults_to_chat_with_default_backend() {
        let cli = Cli::parse(Vec::<String>::new()).unwrap();
        assert_eq!(cli.command, Command::Chat);
        assert_eq!(cli.base_url, DEFAULT_BASE_URL);
        assert_eq!(cli.model, DEFAULT_MODEL);
        assert_eq!(cli.tool_calling, ToolCallingArg::None);
    }

    #[test]
    fn parses_tool_calling_modes_and_maps_to_backend() {
        use dc_model::{ModelBackend, ToolCalling};
        let native = Cli::parse(["--tool-calling", "native"]).unwrap();
        assert_eq!(native.tool_calling, ToolCallingArg::Native);
        assert_eq!(
            native.backend().capabilities().tool_calling,
            ToolCalling::OpenAiStyle
        );

        let gbnf = Cli::parse(["--tool-calling", "gbnf"]).unwrap();
        assert_eq!(
            gbnf.backend().capabilities().tool_calling,
            ToolCalling::Gbnf
        );

        assert!(Cli::parse(["--tool-calling", "bogus"]).is_err());
    }

    #[test]
    fn parses_run_with_task_verify_and_plan() {
        let cli = Cli::parse([
            "run",
            "make",
            "the",
            "test",
            "pass",
            "--verify",
            "sh test.sh",
            "--plan",
        ])
        .unwrap();
        match &cli.command {
            Command::Run { task } => assert_eq!(task, "make the test pass"),
            other => panic!("expected Run, got {other:?}"),
        }
        assert_eq!(cli.verify_command.as_deref(), Some("sh test.sh"));
        assert!(cli.plan_first);
        // The config reflects the flags.
        let cfg = cli.agent_config();
        assert_eq!(cfg.verify_command.as_deref(), Some("sh test.sh"));
        assert!(cfg.plan_first);
    }

    #[test]
    fn run_requires_a_task() {
        assert!(Cli::parse(["run"]).is_err());
    }

    #[test]
    fn parses_swarm_with_orchestrator_and_workers() {
        let cli = Cli::parse([
            "swarm",
            "add validation",
            "--base-url",
            "http://localhost:11435/v1",
            "--model",
            "coder-0",
            "--orchestrator-url",
            "http://localhost:11434/v1",
            "--orchestrator",
            "advisor-e4b",
            "--max-workers",
            "3",
            "--max-retries",
            "4",
            "--frozen",
            "tests/test_a.py, tests\\test_b.py",
            "--verify",
            "pytest -q",
        ])
        .unwrap();
        match &cli.command {
            // The flags after the task must be peeled off, not swept into the goal
            // (regression: `--max-retries` once leaked into the task string).
            Command::Swarm { task } => assert_eq!(task, "add validation"),
            other => panic!("expected Swarm, got {other:?}"),
        }
        assert_eq!(cli.model, "coder-0"); // workers
        assert_eq!(cli.orchestrator_model.as_deref(), Some("advisor-e4b"));
        assert_eq!(
            cli.orchestrator_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(cli.max_workers, 3);
        assert_eq!(cli.max_subtask_retries, 4);
        // `--frozen` parses + normalizes separators, and survives the task-peel.
        assert_eq!(cli.frozen_paths, vec!["tests/test_a.py", "tests/test_b.py"]);
        // The swarm config carries the verify command (gates integration) + workers.
        let sc = cli.swarm_config();
        assert_eq!(sc.max_workers, 3);
        assert_eq!(sc.max_subtask_retries, 4);
        assert_eq!(sc.frozen_paths, vec!["tests/test_a.py", "tests/test_b.py"]);
        assert_eq!(sc.verify_command.as_deref(), Some("pytest -q"));
    }

    #[test]
    fn staged_subcommand_parses_task_and_verify() {
        // `staged` is the MCP's headless entry; the task must peel cleanly and
        // `--verify` (the per-stage gate override) must survive the peel.
        let cli = Cli::parse(["staged", "wire the invite list", "--verify", "cargo check"]).unwrap();
        match &cli.command {
            Command::Staged { task } => assert_eq!(task, "wire the invite list"),
            other => panic!("expected Staged, got {other:?}"),
        }
        assert_eq!(cli.verify_command.as_deref(), Some("cargo check"));
    }

    #[test]
    fn frozen_list_trims_normalizes_and_drops_empties() {
        assert_eq!(
            parse_frozen_list("a.py, b.py ,,c\\d.py"),
            vec!["a.py", "b.py", "c/d.py"]
        );
        assert!(parse_frozen_list("   ").is_empty());
    }

    #[test]
    fn test_file_heuristic_matches_pytest_conventions() {
        assert!(looks_like_test_file("test_clamp.py"));
        assert!(looks_like_test_file("clamp_test.py"));
        assert!(looks_like_test_file("tests/anything.py"));
        assert!(looks_like_test_file("pkg/tests/util.py"));
        // Not tests.
        assert!(!looks_like_test_file("clamp.py"));
        assert!(!looks_like_test_file("contest.py")); // not test_/_test
        assert!(!looks_like_test_file("tests/data.json")); // under tests/ but not .py
    }

    #[test]
    fn detect_test_files_finds_tests_one_level_and_in_tests_dir() {
        let ws = std::env::temp_dir().join(format!(
            "dc-cli-detect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(ws.join("tests")).unwrap();
        std::fs::write(ws.join("clamp.py"), "x=1\n").unwrap();
        std::fs::write(ws.join("test_clamp.py"), "x=1\n").unwrap();
        std::fs::write(ws.join("tests").join("test_more.py"), "x=1\n").unwrap();
        std::fs::write(ws.join("tests").join("helpers.py"), "x=1\n").unwrap(); // under tests/

        let mut found = detect_test_files(&ws);
        found.sort();
        assert_eq!(
            found,
            vec!["test_clamp.py", "tests/helpers.py", "tests/test_more.py"],
            "should freeze test_*.py and everything under tests/, not clamp.py"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn parses_swarm_cli_and_json_flags() {
        // `--cli` in the swarm tail switches to the line renderer.
        let cli = Cli::parse(["swarm", "add a test", "--cli"]).unwrap();
        assert!(cli.cli, "--cli should set the line-render flag");
        assert!(!cli.json);

        // As a top-level flag too (flags may appear in any order, spec 00).
        let cli = Cli::parse(["--cli", "swarm", "add a test"]).unwrap();
        assert!(cli.cli);

        // `--json` is parsed independently; the `--json ⇒ cli` implication is
        // applied at the call site, not here, so a bare --json leaves cli false.
        let cli = Cli::parse(["swarm", "add a test", "--json"]).unwrap();
        assert!(cli.json);
        assert!(!cli.cli);
    }

    #[test]
    fn swarm_orchestrator_defaults_to_worker_endpoint() {
        let cli =
            Cli::parse(["swarm", "task", "--model", "m", "--base-url", "http://x/v1"]).unwrap();
        assert_eq!(cli.max_workers, 2); // default
                                        // No --orchestrator-* → orchestrator() reuses base_url/model (built ok).
        let _ = cli.orchestrator();
        assert!(cli.orchestrator_model.is_none());
    }

    #[test]
    fn parses_advisor_and_builds_a_second_backend() {
        use dc_model::ModelBackend;
        // As a top-level flag and inside a `run`/`serve` tail.
        let cli = Cli::parse([
            "run",
            "fix it",
            "--model",
            "gemma4:e2b",
            "--advisor",
            "gemma4:e4b",
        ])
        .unwrap();
        assert_eq!(cli.advisor_model.as_deref(), Some("gemma4:e4b"));
        let advisor = cli.advisor().expect("advisor backend");
        assert_eq!(advisor.name(), "openai-compat");
        // No --advisor → no advisor backend.
        assert!(Cli::parse(["run", "x"]).unwrap().advisor().is_none());
    }

    #[test]
    fn parses_doctor_with_overrides_in_any_order() {
        let cli = Cli::parse([
            "--model",
            "qwen2:1.5b",
            "doctor",
            "--base-url",
            "http://host:8000/v1",
        ])
        .unwrap();
        assert_eq!(cli.command, Command::Doctor);
        assert_eq!(cli.model, "qwen2:1.5b");
        assert_eq!(cli.base_url, "http://host:8000/v1");
    }

    #[test]
    fn plan_is_autonomous_by_default_and_gated_with_interactive() {
        let auto = Cli::parse(["plan", "build a parser"]).unwrap();
        assert_eq!(
            auto.command,
            Command::Plan {
                task: "build a parser".to_string(),
                interactive: false,
            }
        );
        // Both spellings turn on the human checkpoints; the flag is peeled out of
        // the greedily-collected task.
        for flag in ["--interactive", "--gate"] {
            let gated = Cli::parse(["plan", "build a parser", flag]).unwrap();
            assert_eq!(
                gated.command,
                Command::Plan {
                    task: "build a parser".to_string(),
                    interactive: true,
                }
            );
        }
    }

    #[test]
    fn ceremony_tier_resolves_to_its_gate_set() {
        use dc_workflow::Ceremony;
        let cli = Cli::parse(["plan", "fix a typo", "--ceremony", "standard"]).unwrap();
        assert_eq!(cli.ceremony, Some(Ceremony::Standard));
        assert_eq!(cli.ceremony_gates(), Ceremony::Standard.gates());
        // A bad tier is a loud error.
        assert!(Cli::parse(["plan", "t", "--ceremony", "lavish"]).is_err());
    }

    #[test]
    fn explicit_gates_override_the_tier_and_parse_slugs() {
        use dc_workflow::{Phase, PhaseSet};
        let cli = Cli::parse([
            "plan",
            "do it",
            "--ceremony",
            "minimal",
            "--gates",
            "specs,stage-breakdown",
        ])
        .unwrap();
        // --gates wins over --ceremony.
        assert_eq!(
            cli.ceremony_gates(),
            PhaseSet::of([Phase::Specs, Phase::StageBreakdown])
        );
    }

    #[test]
    fn gates_with_unknown_slug_is_an_error() {
        let err = Cli::parse(["plan", "t", "--gates", "specs,frobnicate"]).unwrap_err();
        assert!(err.to_string().contains("unknown phase"), "got: {err}");
    }

    #[test]
    fn bare_interactive_gates_every_phase() {
        use dc_workflow::Ceremony;
        // No ceremony/gates flag → ceremony_gates() defaults to Full (today's
        // behavior preserved), and the run is gated.
        let cli = Cli::parse(["plan", "t", "--interactive"]).unwrap();
        assert!(cli.ceremony.is_none() && cli.gates.is_none());
        assert_eq!(cli.ceremony_gates(), Ceremony::Full.gates());
        assert!(cli.plan_is_gated(true));
    }

    #[test]
    fn no_ceremony_flags_runs_autonomously() {
        let cli = Cli::parse(["plan", "t"]).unwrap();
        // interactive=false and no policy → not gated.
        assert!(!cli.plan_is_gated(false));
    }

    #[test]
    fn ceremony_and_gates_imply_interactive() {
        // Naming a policy turns the gates on even without --interactive.
        let tier = Cli::parse(["plan", "t", "--ceremony", "minimal"]).unwrap();
        assert!(tier.plan_is_gated(false));
        let explicit = Cli::parse(["plan", "t", "--gates", "specs"]).unwrap();
        assert!(explicit.plan_is_gated(false));
    }

    #[test]
    fn parses_json_log_yolo_allow_dry_run_top_level_and_in_run_tail() {
        // Top-level (before the subcommand).
        let top = Cli::parse([
            "--json",
            "--dry-run",
            "--yolo",
            "--allow",
            "cargo test",
            "--log",
            "out.jsonl",
            "run",
            "do it",
        ])
        .unwrap();
        assert!(top.json && top.dry_run && top.yolo);
        assert_eq!(top.allow, vec!["cargo test".to_string()]);
        assert_eq!(top.log.as_deref(), Some("out.jsonl"));

        // In the run tail (after the task) — and --allow repeats.
        let tail = Cli::parse([
            "run",
            "do it",
            "--json",
            "--dry-run",
            "--yolo",
            "--allow",
            "git status",
            "--allow",
            "ls",
            "--log",
            "x.jsonl",
        ])
        .unwrap();
        assert!(tail.json && tail.dry_run && tail.yolo);
        assert_eq!(tail.allow, vec!["git status".to_string(), "ls".to_string()]);
        assert_eq!(tail.log.as_deref(), Some("x.jsonl"));
        match &tail.command {
            Command::Run { task } => assert_eq!(task, "do it"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn safety_flags_populate_the_permission_policy_and_dry_run() {
        let cli = Cli::parse(["run", "x", "--yolo", "--allow", "cargo test", "--dry-run"]).unwrap();
        let cfg = cli.agent_config();
        assert!(cfg.permission.allow_shell, "--yolo → allow_shell");
        assert_eq!(
            cfg.permission.shell_allowlist,
            vec!["cargo test".to_string()]
        );
        assert!(cfg.dry_run, "--dry-run → dry_run");

        // Defaults: no flags → conservative policy, no dry-run, no verbose.
        let plain = Cli::parse(["run", "x"]).unwrap().agent_config();
        assert!(!plain.permission.allow_shell);
        assert!(plain.permission.shell_allowlist.is_empty());
        assert!(!plain.dry_run);
        assert!(!plain.verbose);
    }

    #[test]
    fn verbose_flag_parses_both_spellings_and_positions_and_wires_config() {
        for flag in ["--verbose", "-v"] {
            // Top-level and in the run tail.
            let top = Cli::parse([flag, "run", "x"]).unwrap();
            assert!(top.verbose, "top-level {flag}");
            let tail = Cli::parse(["run", "x", flag]).unwrap();
            assert!(tail.verbose, "run-tail {flag}");
            assert!(tail.agent_config().verbose, "{flag} → AgentConfig.verbose");
        }
        assert!(!Cli::parse(["run", "x"]).unwrap().verbose);
    }

    #[test]
    fn parses_replay_and_requires_an_id() {
        let cli = Cli::parse(["replay", "1718000000000"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Replay {
                session: "1718000000000".to_string()
            }
        );
        assert!(Cli::parse(["replay"]).is_err());
    }

    #[test]
    fn session_log_path_defaults_under_dot_dir_and_honors_override() {
        let ws = std::path::Path::new("/tmp/ws");
        // Default: .dumb-coder/sessions/<millis>.jsonl, id is the numeric stem.
        let (path, id) = session_log_path(ws, None);
        assert!(path.ends_with(format!("{id}.jsonl")), "{path:?}");
        assert!(path.to_string_lossy().contains("sessions"), "{path:?}");
        assert!(id.chars().all(|c| c.is_ascii_digit()), "id={id}");
        // Override wins; id is derived from the file stem.
        let (p2, id2) = session_log_path(ws, Some("logs/my-run.jsonl"));
        assert_eq!(p2, std::path::PathBuf::from("logs/my-run.jsonl"));
        assert_eq!(id2, "my-run");
    }

    #[test]
    fn resolve_replay_path_handles_bare_id_and_suffix() {
        let ws = std::path::Path::new("/tmp/ws");
        let from_id = resolve_replay_path(ws, "123");
        assert!(
            from_id.ends_with("sessions/123.jsonl") || from_id.ends_with("sessions\\123.jsonl")
        );
        // A .jsonl-suffixed bare id resolves to the same place (not doubled).
        let from_suffixed = resolve_replay_path(ws, "123.jsonl");
        assert_eq!(from_id, from_suffixed);
    }

    #[test]
    fn help_is_recognized() {
        assert_eq!(Cli::parse(["help"]).unwrap().command, Command::Help);
        assert_eq!(Cli::parse(["--help"]).unwrap().command, Command::Help);
        assert_eq!(Cli::parse(["-h"]).unwrap().command, Command::Help);
    }

    #[test]
    fn unknown_argument_is_an_error_not_silently_ignored() {
        let err = Cli::parse(["--frobnicate"]).unwrap_err();
        assert!(err.to_string().contains("unknown argument"), "got: {err}");
    }

    #[test]
    fn flag_without_value_errors() {
        assert!(Cli::parse(["--model"]).is_err());
        assert!(Cli::parse(["--base-url"]).is_err());
    }

    #[test]
    fn doctor_report_shows_reachable_status_and_budget() {
        let cli = Cli::parse(["doctor"]).unwrap();
        let caps = cli.backend().capabilities();
        let report = doctor_report(&cli, &caps, &Ok(()));
        assert!(report.contains("reachable ✓"), "got: {report}");
        assert!(report.contains("8192 tokens"), "got: {report}");
        assert!(report.contains(DEFAULT_MODEL), "got: {report}");
    }

    #[test]
    fn doctor_report_surfaces_an_unreachable_backend() {
        let cli = Cli::parse(["doctor"]).unwrap();
        let caps = cli.backend().capabilities();
        let report = doctor_report(
            &cli,
            &caps,
            &Err(DcError::Backend("connection refused".to_string())),
        );
        assert!(report.contains("UNREACHABLE ✗"), "got: {report}");
        assert!(report.contains("connection refused"), "got: {report}");
    }
}
