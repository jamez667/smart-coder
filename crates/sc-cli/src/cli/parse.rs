//! Argv → [`Cli`]. A hand-rolled parser (no clap): the grammar is small, and an
//! unknown token is an error rather than silently ignored (spec 00 — fail loud).
//!
//! Two passes exist because `run`/`serve`/`swarm`/`plan`/`staged` greedily consume
//! the rest of argv as their task: [`Cli::parse`] handles the top level, and
//! [`split_run_args`] peels the known flags back out of the collected task words.

use sc_proto::{DcError, Result};

use super::types::{Cli, Command, QueueAction, ToolCallingArg, DEFAULT_BASE_URL, DEFAULT_MODEL};

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
        let mut ceremony: Option<sc_workflow::Ceremony> = None;
        let mut gates: Option<sc_workflow::PhaseSet> = None;
        let mut advisor_model = None;
        let mut advisor_url = None;
        let mut system_suffix: Option<String> = None;
        let mut orchestrator_model = None;
        let mut orchestrator_url = None;
        let mut api_key = None;
        let mut orchestrator_key = None;
        let mut max_workers = 2usize;
        let mut max_subtask_retries = 2usize;
        let mut frozen_paths: Vec<String> = Vec::new();
        let mut review = false;
        let mut review_action = sc_swarm::ReviewAction::default();
        let mut review_gate = sc_swarm::Severity::High;
        let mut trace_check = false;
        let mut json = false;
        let mut log: Option<String> = None;
        let mut yolo = false;
        let mut allow: Vec<String> = Vec::new();
        let mut dry_run = false;
        let mut verbose = false;
        let mut cli_render = false;
        let mut port: u16 = 8177;
        let mut no_token = false;

        // Collected rather than lazily mapped so a greedy subcommand can hand back
        // the flags it did not own and have them re-enter this same loop.
        let mut it = args
            .into_iter()
            .map(Into::into)
            .collect::<Vec<String>>()
            .into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "doctor" if command.is_none() => command = Some(Command::Doctor),
                "trace" if command.is_none() => command = Some(Command::Trace { check: false }),
                "chat" if command.is_none() => command = Some(Command::Chat),
                "remote" if command.is_none() => command = Some(Command::Remote),
                "comply" if command.is_none() => command = Some(Command::Comply { pack: None }),
                "--list-packs" if command.is_none() => command = Some(Command::ListPacks),
                // Loopback-only, read-only dashboard: the 64-char token in the
                // URL is friction for a local run. Opt-in only, never a default.
                "--no-token" => no_token = true,
                // `trace --check` — the CI gate (spec 17). Held in a local rather
                // than mutated onto the command, so `--check trace` parses the
                // same as `trace --check`: a gate flag that silently does nothing
                // because it came first is a gate that is not running.
                "--check" => trace_check = true,
                "comply-lint" if command.is_none() => {
                    command = Some(Command::ComplyLint { pack: None })
                }
                "comply-eval" if command.is_none() => {
                    command = Some(Command::ComplyEval { models: Vec::new() })
                }
                "comply-export" if command.is_none() => {
                    command = Some(Command::ComplyExport { out: None })
                }
                "--out" => {
                    let dir = it.next().ok_or_else(|| {
                        DcError::Eval(
                            "--out requires a directory, e.g. `--out docs/compliance`".to_string(),
                        )
                    })?;
                    match &mut command {
                        Some(Command::ComplyExport { out }) => *out = Some(dir),
                        _ => {
                            return Err(DcError::Eval(
                                "--out only applies to `comply-export`".to_string(),
                            ))
                        }
                    }
                }
                // Repeatable, so two models can be compared in one run. Accepts
                // `model` or `model@base_url` — the local server and Gemini live
                // at different endpoints, and a comparison needs both.
                "--author-model" => {
                    let spec = it.next().ok_or_else(|| {
                        DcError::Eval(
                            "--author-model requires a model name, optionally with an \
                             endpoint: `--author-model qwen3-coder-30b@http://localhost:11435/v1`"
                                .to_string(),
                        )
                    })?;
                    match &mut command {
                        Some(Command::ComplyEval { models }) => models.push(spec),
                        _ => {
                            return Err(DcError::Eval(
                                "--author-model only applies to `comply-eval`".to_string(),
                            ))
                        }
                    }
                }
                // `--pack` only means anything to the compliance subcommands;
                // accepted after the subcommand so
                // `smart-coder comply --pack soc2.toml` reads naturally.
                "--pack" => {
                    let path = it.next().ok_or_else(|| {
                        DcError::Eval(
                            "--pack requires a path to a framework pack, e.g. \
                             `--pack crates/sc-comply/packs/soc2-tsc`"
                                .to_string(),
                        )
                    })?;
                    match &mut command {
                        Some(Command::Comply { pack }) | Some(Command::ComplyLint { pack }) => {
                            *pack = Some(path)
                        }
                        _ => {
                            return Err(DcError::Eval(
                                "--pack only applies to `comply` and `comply-lint`".to_string(),
                            ))
                        }
                    }
                }
                // `queue <action> [args…]` — the task queue (spec 19). Its actions
                // take the rest of argv, so it is parsed as a unit rather than
                // leaving loose words for the top-level loop to trip over.
                //
                // Backend flags still have to work, though: `queue run
                // --orchestrator-url …` silently probing the default endpoint is a
                // flag that looks accepted and does nothing (found live). So the
                // action parser hands back what it did not own, and those tokens go
                // through this same loop.
                "queue" if command.is_none() => {
                    let rest: Vec<String> = it.by_ref().collect();
                    let (action, leftover) = parse_queue_action(rest)?;
                    command = Some(Command::Queue { action });
                    it = leftover.into_iter();
                }
                "replay" if command.is_none() => {
                    let session = it.next().ok_or_else(|| {
                        DcError::Eval(
                            "replay requires a session id or log path, e.g. \
                             `smart-coder replay 1718000000000`"
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
                            "{kind} requires a task, e.g. `smart-coder {kind} \"add a test\"`"
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
                    if parsed.api_key.is_some() {
                        api_key = parsed.api_key;
                    }
                    if parsed.orchestrator_key.is_some() {
                        orchestrator_key = parsed.orchestrator_key;
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
                    if parsed.review {
                        review = true;
                    }
                    if let Some(a) = parsed.review_action {
                        review_action = a;
                    }
                    if let Some(s) = parsed.review_gate {
                        review_gate = s;
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
                "--key" => {
                    api_key = Some(
                        it.next()
                            .ok_or_else(|| DcError::Eval("--key requires a token".to_string()))?,
                    );
                }
                "--orchestrator-key" => {
                    orchestrator_key = Some(it.next().ok_or_else(|| {
                        DcError::Eval("--orchestrator-key requires a token".to_string())
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
                "--port" => {
                    port = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .filter(|n| *n != 0)
                        .ok_or_else(|| {
                            DcError::Eval("--port requires a port number 1-65535".to_string())
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
                // Post-integration review (spec 16). Off unless asked for: it is
                // model calls over every integrated diff, and a user pays for them.
                "--review" => review = true,
                "--review-action" => {
                    let v = it.next().ok_or_else(|| {
                        DcError::Eval("--review-action requires report|gate|retry".to_string())
                    })?;
                    review_action = parse_review_action(&v)?;
                    // Naming what to DO with findings implies wanting them.
                    review = true;
                }
                "--review-gate" => {
                    let v = it.next().ok_or_else(|| {
                        DcError::Eval("--review-gate requires low|medium|high".to_string())
                    })?;
                    review_gate = sc_swarm::Severity::parse(&v).ok_or_else(|| {
                        DcError::Eval(format!("--review-gate: unknown severity {v:?}"))
                    })?;
                    review = true;
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
                        "unknown argument: {other:?} (try `smart-coder help`)"
                    )));
                }
            }
        }

        // No auto `/no_think`. Early Qwen3 reasoning models needed it to avoid burning the
        // budget on a `<think>` block, but the current coder model (qwen3-coder-30b) has no
        // thinking mode (confirmed live: zero <think> tags) — so it was dead prompt text the
        // model ignored. Pass `--no-think` explicitly if you run a thinking model that needs it.

        // Fall back to the conventional GEMINI_API_KEY env var when no key flag was given, so a
        // Gemini planner/coder lights up from the environment without repeating the token on the
        // command line.
        let env_key = std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let api_key = api_key.or_else(|| env_key.clone());
        let orchestrator_key = orchestrator_key.or_else(|| api_key.clone()).or(env_key);

        // `--check` belongs to `trace`. Silently ignoring it elsewhere would let
        // a user believe a gate was in place that never ran (spec 00 — fail loud).
        let command = match (command, trace_check) {
            (Some(Command::Trace { .. }), check) => Some(Command::Trace { check }),
            (other, true) => {
                let _ = other;
                return Err(DcError::Eval("--check only applies to `trace`".to_string()));
            }
            (other, false) => other,
        };

        Ok(Cli {
            command: command.unwrap_or(Command::Chat),
            base_url,
            model,
            tool_calling,
            api_key,
            verify_command,
            plan_first,
            advisor_model,
            advisor_url,
            system_suffix,
            orchestrator_model,
            orchestrator_url,
            orchestrator_key,
            max_workers,
            max_subtask_retries,
            frozen_paths,
            review,
            review_action,
            review_gate,
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
            port,
            no_token,
        })
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
    /// `--key` — bearer token for the coder endpoint (e.g. the Gemini API key).
    api_key: Option<String>,
    /// `--orchestrator-key` — bearer token for the planner endpoint (the Gemini key).
    orchestrator_key: Option<String>,
    max_workers: Option<usize>,
    /// `--max-retries N` — per-subtask retry cap for `swarm` (spec 08).
    max_subtask_retries: Option<usize>,
    /// `--frozen a.py,b.py` — frozen contract-test paths for `swarm` (spec 08/11).
    frozen_paths: Option<Vec<String>>,
    /// `--review` — post-integration review over each integrated diff (spec 16).
    review: bool,
    /// `--review-action report|gate|retry` — what happens to a finding.
    review_action: Option<sc_swarm::ReviewAction>,
    /// `--review-gate low|medium|high` — where a corroborated finding stops the run.
    review_gate: Option<sc_swarm::Severity>,
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
    ceremony: Option<sc_workflow::Ceremony>,
    /// `plan` explicit gate set: `--gates specs,architecture,…` (overrides tier).
    gates: Option<sc_workflow::PhaseSet>,
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
    let mut api_key = None;
    let mut orchestrator_key = None;
    let mut max_workers = None;
    let mut max_subtask_retries = None;
    let mut frozen_paths = None;
    let mut review = false;
    let mut review_action = None;
    let mut review_gate = None;
    let mut no_think = false;
    let mut plan = false;
    let mut interactive = false;
    let mut think_base: Option<bool> = None;
    let mut think_steps: Vec<(String, bool)> = Vec::new();
    let mut ceremony: Option<sc_workflow::Ceremony> = None;
    let mut gates: Option<sc_workflow::PhaseSet> = None;
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
            "--key" => api_key = Some(need(&mut it, "--key")?),
            "--orchestrator-key" => orchestrator_key = Some(need(&mut it, "--orchestrator-key")?),
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
            // Post-integration review (spec 16). Naming an action or a gating
            // severity implies wanting the review that produces them.
            "--review" => review = true,
            "--review-action" => {
                review_action = Some(parse_review_action(&need(&mut it, "--review-action")?)?);
                review = true;
            }
            "--review-gate" => {
                let v = need(&mut it, "--review-gate")?;
                review_gate = Some(sc_swarm::Severity::parse(&v).ok_or_else(|| {
                    DcError::Eval(format!(
                        "--review-gate: unknown severity {v:?} (expected low, medium or high)"
                    ))
                })?);
                review = true;
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
                ceremony = Some(sc_workflow::Ceremony::parse(&tier).ok_or_else(|| {
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
        api_key,
        orchestrator_key,
        max_workers,
        max_subtask_retries,
        frozen_paths,
        review,
        review_action,
        review_gate,
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
/// [`sc_workflow::PhaseSet`]. An unknown slug is an error (fail loud, spec 00).
fn parse_gate_set(list: &str) -> Result<sc_workflow::PhaseSet> {
    let mut phases = Vec::new();
    for raw in list.split(',') {
        let slug = raw.trim();
        if slug.is_empty() {
            continue;
        }
        let phase = sc_workflow::Phase::from_slug(slug).ok_or_else(|| {
            DcError::Eval(format!(
                "--gates: unknown phase {slug:?} (expected one of: {})",
                sc_workflow::Phase::ALL
                    .iter()
                    .map(|p| p.slug())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        phases.push(phase);
    }
    Ok(sc_workflow::PhaseSet::of(phases))
}

/// Parse a `--frozen a.py,b.py` list into trimmed, non-empty, slash-normalized
/// paths (so `tests\a.py` and `tests/a.py` compare equal downstream, matching
/// `sc_swarm`'s `is_frozen`).
fn parse_frozen_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| s.trim().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .collect()
}

/// `--review-action report|gate|retry` — what happens to a finding (spec 16), in
/// increasing order of intervention.
fn parse_review_action(v: &str) -> Result<sc_swarm::ReviewAction> {
    match v.trim().to_ascii_lowercase().as_str() {
        "report" => Ok(sc_swarm::ReviewAction::Report),
        "gate" => Ok(sc_swarm::ReviewAction::Gate),
        "retry" => Ok(sc_swarm::ReviewAction::Retry),
        other => Err(DcError::Eval(format!(
            "--review-action: unknown action {other:?} (expected report, gate or retry)"
        ))),
    }
}

/// Parse `queue <action> [args…]`.
///
/// Errors name the usage rather than just refusing, because these are typed by a
/// developer at a terminal who should not have to go and read the help text to
/// recover from a missing argument.
fn parse_queue_action(rest: Vec<String>) -> Result<(QueueAction, Vec<String>)> {
    let mut it = rest.into_iter();
    let action = it.next().ok_or_else(|| {
        DcError::Eval(
            "queue needs an action: file | list | run | approve | send-back | \
             discard | show | feedback | ack | repos | add-repo | forget-repo"
                .to_string(),
        )
    })?;

    // `--repo <name>` may appear anywhere after the action; everything else is
    // positional. Pulling it out first keeps `file` able to take free text.
    // Anything starting `--` that is not ours is handed BACK rather than swallowed
    // into the free text — that is what lets `queue run --orchestrator-url …` reach
    // the backend config instead of looking accepted and doing nothing (found live).
    let mut repo: Option<String> = None;
    let mut kind: Option<sc_daemon::IntakeKind> = None;
    let mut all = false;
    let mut words: Vec<String> = Vec::new();
    let mut leftover: Vec<String> = Vec::new();
    while let Some(w) = it.next() {
        match w.as_str() {
            "--all" => all = true,
            "--kind" => {
                let raw = it
                    .next()
                    .ok_or_else(|| DcError::Eval("--kind needs a value".to_string()))?;
                kind = Some(sc_daemon::IntakeKind::parse(&raw).ok_or_else(|| {
                    DcError::Eval(format!(
                        "unknown kind {raw:?} — expected bug, feature, improvement or \
                         feedback. Defaulting silently would return a feature-shaped \
                         spec for a crash."
                    ))
                })?);
            }
            "--repo" => {
                repo =
                    Some(it.next().ok_or_else(|| {
                        DcError::Eval("--repo needs a repository name".to_string())
                    })?)
            }
            other if other.starts_with("--") => {
                leftover.push(w);
                // Keep a flag's value adjacent to it: the top-level loop reads the
                // pair together. A value-less flag simply passes through, and a
                // stray value becomes an unknown token there — which is the loud
                // failure we want rather than a silent misparse.
                if let Some(next) = it.next() {
                    leftover.push(next);
                }
            }
            _ => words.push(w),
        }
    }
    let joined = words.join(" ");
    let first = words.first().cloned().unwrap_or_default();

    match action.as_str() {
        "file" => {
            if joined.trim().is_empty() {
                return Err(DcError::Eval(
                    "queue file needs the request text, e.g. \
                     `smart-coder queue file \"add seat types\" --repo city`"
                        .to_string(),
                ));
            }
            let repo = repo.ok_or_else(|| {
                DcError::Eval(
                    "queue file needs --repo <name>. A repository is chosen from the \
                     daemon's configured set, never typed as a path — run \
                     `smart-coder queue repos` to see them."
                        .to_string(),
                )
            })?;
            Ok((
                QueueAction::File {
                    text: joined,
                    repo,
                    // Feature is the default because it is the commonest filing and
                    // the least surprising to get back; an *unknown* kind still
                    // errors rather than falling through to it.
                    kind: kind.unwrap_or_default(),
                },
                leftover,
            ))
        }
        "list" => Ok((QueueAction::List, leftover)),
        "run" => Ok((QueueAction::Run, leftover)),
        "repos" => Ok((QueueAction::Repos, leftover)),
        "feedback" => Ok((QueueAction::Feedback { repo, all }, leftover)),
        "ack" => {
            let id = require_id(&first, "ack")?;
            let repo = repo.ok_or_else(|| {
                DcError::Eval(
                    "queue ack needs --repo <name>: feedback is stored per repository".to_string(),
                )
            })?;
            Ok((QueueAction::AckFeedback { repo, id }, leftover))
        }
        "approve" => Ok((
            QueueAction::Approve {
                id: require_id(&first, "approve")?,
            },
            leftover,
        )),
        "discard" => Ok((
            QueueAction::Discard {
                id: require_id(&first, "discard")?,
            },
            leftover,
        )),
        "show" => Ok((
            QueueAction::Show {
                id: require_id(&first, "show")?,
            },
            leftover,
        )),
        "send-back" => {
            let id = require_id(&first, "send-back")?;
            let notes = words[1..].join(" ");
            if notes.trim().is_empty() {
                return Err(DcError::Eval(
                    "queue send-back needs a note saying what to change — without one \
                     the redraft has nothing to go on and will likely produce the \
                     same spec"
                        .to_string(),
                ));
            }
            Ok((QueueAction::SendBack { id, notes }, leftover))
        }
        "add-repo" => {
            let name = require_id(&first, "add-repo")?;
            let path = words.get(1).cloned().ok_or_else(|| {
                DcError::Eval(
                    "queue add-repo needs a name and a path, e.g. \
                     `smart-coder queue add-repo city ../city`"
                        .to_string(),
                )
            })?;
            Ok((QueueAction::AddRepo { name, path }, leftover))
        }
        "forget-repo" => Ok((
            QueueAction::ForgetRepo {
                name: require_id(&first, "forget-repo")?,
            },
            leftover,
        )),
        other => Err(DcError::Eval(format!(
            "unknown queue action {other:?} — expected file, list, run, approve, \
             send-back, discard, show, feedback, ack, repos, add-repo or forget-repo"
        ))),
    }
}

/// A required positional argument, named in the error so a bare action says what
/// it was missing.
fn require_id(value: &str, action: &str) -> Result<String> {
    if value.trim().is_empty() {
        Err(DcError::Eval(format!(
            "queue {action} needs a task id — run `smart-coder queue list` to see them"
        )))
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod private_tests {
    use super::{parse_frozen_list, parse_review_action};

    #[test]
    fn review_action_parses_the_three_outcomes_and_rejects_anything_else() {
        assert_eq!(
            parse_review_action("retry").unwrap(),
            sc_swarm::ReviewAction::Retry
        );
        assert_eq!(
            parse_review_action(" GATE ").unwrap(),
            sc_swarm::ReviewAction::Gate
        );
        // An unrecognised action must be an error, never a silent fallback to
        // `report` — a user asking to gate and quietly getting report-only would
        // believe a gate was in place that never was.
        assert!(parse_review_action("fix").is_err());
    }

    #[test]
    fn frozen_list_trims_normalizes_and_drops_empties() {
        assert_eq!(
            parse_frozen_list("a.py, b.py ,,c\\d.py"),
            vec!["a.py", "b.py", "c/d.py"]
        );
        assert!(parse_frozen_list("   ").is_empty());
    }
}
