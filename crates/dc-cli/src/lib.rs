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
        let mut advisor_model = None;
        let mut advisor_url = None;
        let mut system_suffix: Option<String> = None;
        let mut orchestrator_model = None;
        let mut orchestrator_url = None;
        let mut max_workers = 2usize;

        let mut it = args.into_iter().map(Into::into);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "doctor" if command.is_none() => command = Some(Command::Doctor),
                "chat" if command.is_none() => command = Some(Command::Chat),
                // `run`/`serve`/`swarm <task...>`: the rest forms the task + flags.
                "run" | "serve" | "swarm" if command.is_none() => {
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
                "--no-think" => system_suffix = Some("/no_think".to_string()),
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

        // Qwen3 models default to a reasoning mode that eats the token budget and
        // returns empty content; `/no_think` disables it. Auto-apply unless the
        // user set a suffix explicitly.
        if system_suffix.is_none() && model.to_ascii_lowercase().contains("qwen3") {
            system_suffix = Some("/no_think".to_string());
        }

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
        })
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
        match self.tool_calling {
            ToolCallingArg::None => OpenAiBackend::new(self.base_url.clone(), self.model.clone()),
            ToolCallingArg::Native => {
                OpenAiBackend::new(self.base_url.clone(), self.model.clone()).with_native_tools()
            }
            ToolCallingArg::Gbnf => {
                OpenAiBackend::llama_cpp(self.base_url.clone(), self.model.clone())
            }
        }
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
    no_think: bool,
    plan: bool,
    // Global flags may also follow the task; capture them so they aren't swept
    // into the task string.
    base_url: Option<String>,
    model: Option<String>,
    tool_calling: Option<ToolCallingArg>,
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
    let mut no_think = false;
    let mut plan = false;
    let mut base_url = None;
    let mut model = None;
    let mut tool_calling = None;
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
        no_think,
        plan,
        base_url,
        model,
        tool_calling,
    })
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
  swarm only (workers use --base-url/--model):
    --orchestrator MODEL  The model that decomposes the task    [default: --model]
    --orchestrator-url U  Endpoint for the orchestrator         [default: --base-url]
    --max-workers N       Max parallel workers                  [default: 2]

EXAMPLES:
    dumb-coder doctor
    dumb-coder run \"make the failing test in is_even pass\" --verify \"sh test.sh\"
    dumb-coder serve \"fix the bug in parse_config\" --verify \"cargo test\"
    dumb-coder swarm \"add validation and a test\" --verify \"python -m pytest -q\" \\
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

#[cfg(test)]
mod tests {
    use super::*;

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
            "--verify",
            "pytest -q",
        ])
        .unwrap();
        match &cli.command {
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
        // The swarm config carries the verify command (gates integration) + workers.
        let sc = cli.swarm_config();
        assert_eq!(sc.max_workers, 3);
        assert_eq!(sc.verify_command.as_deref(), Some("pytest -q"));
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
