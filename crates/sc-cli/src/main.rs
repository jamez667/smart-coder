//! `smart-coder` binary — a thin I/O shell over [`sc_cli`] (spec 06, M0).
//!
//! Parses args, then either prints the `doctor` report or runs a line-oriented
//! chat REPL. All the testable logic is in the library; this file is just stdin/
//! stdout plumbing.

use std::io::{self, Write};
use std::process::ExitCode;

use sc_cli::{doctor_report, probe, usage, Cli, Command};
use sc_model::{GenerateRequest, Message, ModelBackend};

fn main() -> ExitCode {
    // Load a root `.env` (if present) before parsing, so a key kept there — e.g. GEMINI_API_KEY
    // for a Gemini planner/coder — is visible to the CLI's env fallback. Real env vars still win.
    sc_model::load_dotenv();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match Cli::parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{}", usage());
            return ExitCode::FAILURE;
        }
    };

    match &cli.command {
        Command::Help => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Command::Doctor => run_doctor(&cli),
        Command::Chat => run_chat(&cli),
        Command::Run { task } if cli.json => run_task_json(&cli, task.clone()),
        Command::Run { task } => run_task(&cli, task.clone()),
        Command::Serve { task } => serve_task(&cli, task.clone()),
        Command::Remote => remote_task(&cli),
        Command::Comply { pack } => comply_task(&cli, pack.clone()),
        Command::ComplyLint { pack } => comply_lint(pack.clone()),
        Command::ListPacks => {
            print!("{}", sc_comply::registry::listing());
            ExitCode::SUCCESS
        }
        Command::ComplyEval { models } => comply_eval(&cli, models.clone()),
        Command::ComplyExport { out } => comply_export(out.clone()),
        Command::Swarm { task } => swarm_task(&cli, task.clone()),
        Command::Plan { task, interactive } => plan_task(&cli, task.clone(), *interactive),
        Command::Staged { task } => staged_task_json(&cli, task.clone()),
        Command::Replay { session } => replay(session.clone()),
    }
}

/// Run the staged planning workflow (spec 09): the orchestrator (T1) plans each
/// phase, workers (T2) write the tests from the Phase-4 coverage plan, and — when a
/// `--verify` command is given — the swarm implements the work decomposition against
/// those tests until the suite is green. Plan artifacts land in `.smart-coder/plan/`.
///
/// `interactive` toggles the human checkpoints: when set, the workflow halts at each
/// phase boundary for an approve/revise/send-back/abort decision (the macro gate of
/// spec 09); otherwise every gate is auto-approved.
fn plan_task(cli: &Cli, task: String, interactive: bool) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let orchestrator = cli.orchestrator();
    let worker = cli.backend();

    // Preflight: a dead/crashed backend would otherwise silently produce empty
    // plan artifacts mid-run. Fail fast with a clear message instead.
    if let Err(e) = sc_cli::preflight(&[("orchestrator", &orchestrator), ("worker", &worker)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let on_phase = |phase: sc_workflow::Phase, content: &str| {
        let preview: String = content.lines().take(8).collect::<Vec<_>>().join("\n");
        println!("\n=== {} ===\n{preview}\n…", phase.title());
    };

    // Autonomous by default; `--interactive`/`--gate`/`--ceremony`/`--gates` put a
    // human at the gates (spec 09). Adaptive ceremony scales *which* phases stop:
    // the resolved gate set decides which phases consult the stdin gate; the rest
    // auto-approve. The gate is fully harness-owned.
    let auto = sc_workflow::AutoApprove;
    let stdin_gate = StdinGate::new(&workspace);
    let gate_set = cli.ceremony_gates();
    let ceremony_gate = sc_workflow::CeremonyGate::new(gate_set, &stdin_gate);
    let gated = cli.plan_is_gated(interactive);
    let gate: &dyn sc_workflow::Gate = if gated { &ceremony_gate } else { &auto };
    if gated {
        let gated_phases: Vec<&str> = gate_set.phases().iter().map(|p| p.title()).collect();
        let tier = cli
            .ceremony
            .map(|c| c.label())
            .unwrap_or(if cli.gates.is_some() {
                "custom"
            } else {
                "full"
            });
        println!("ceremony: {tier} — gating {}", gated_phases.join(", "));
    }

    let outcome = match sc_workflow::run_workflow_gated(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        cli.think_policy(),
        &on_phase,
        gate,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: workflow failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if outcome.aborted {
        println!("\nplan aborted at a checkpoint — approved artifacts kept in .smart-coder/plan/");
        return ExitCode::SUCCESS;
    }

    println!(
        "\nplan complete — 6 phase artifacts in .smart-coder/plan/\n  tests written: {}\n  subtasks for the swarm: {}",
        if outcome.test_files.is_empty() {
            "(none)".to_string()
        } else {
            outcome.test_files.join(", ")
        },
        outcome.board.len()
    );

    // Without a verify command there's nothing to drive the implementation against;
    // stop at the approved plan + frozen tests.
    let Some(_) = cli.verify_command.clone() else {
        println!("(no --verify given; stopping at the plan + tests. Add --verify to build it.)");
        return ExitCode::SUCCESS;
    };
    if outcome.board.is_empty() {
        eprintln!("warning: work decomposition produced no subtasks; nothing to implement");
        return ExitCode::FAILURE;
    }

    // Implement: run the swarm against the workflow's own board, gated by the
    // frozen tests the workers just wrote (the merge may never overwrite them).
    println!("\n=== implementing against the written tests ===");
    let advisor = cli.swarm_advisor();
    let mut swarm_cfg = cli.swarm_config();
    swarm_cfg.frozen_paths = outcome.test_files.clone();
    let sink = sc_swarm::NullSwarmSink;
    let report = sc_swarm::run_swarm_board(
        &orchestrator,
        &worker,
        Some(&advisor as &(dyn sc_model::ModelBackend + Sync)),
        outcome.board,
        &workspace,
        &swarm_cfg,
        &sink,
    );
    println!(
        "\nswarm: {} integrated, {} rejected, {} pending",
        report.done, report.failed, report.pending
    );
    if report.all_done {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The interactive checkpoint gate (spec 09): at each phase boundary it presents the
/// artifact and reads one of approve / revise / send-back / abort from stdin. The
/// artifact is already persisted to disk before we're consulted, so **revise** is
/// "edit the file, then press enter" — we re-read it (the runner picks up the edit).
struct StdinGate {
    workspace: std::path::PathBuf,
}

impl StdinGate {
    fn new(workspace: &std::path::Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }
}

impl sc_workflow::Gate for StdinGate {
    fn decide(
        &self,
        phase: sc_workflow::Phase,
        artifact: &sc_workflow::Artifact,
    ) -> sc_workflow::Decision {
        use sc_workflow::Decision;
        let file = sc_workflow::plan_dir(&self.workspace).join(phase.filename());
        let stdin = io::stdin();
        loop {
            println!(
                "\n⛳ Checkpoint: {} — review {}\n   {} lines. \
                 [a]pprove · [r]evise (edit the file, then enter) · [s]end-back · [x] abort",
                phase.title(),
                file.display(),
                artifact.content.lines().count(),
            );
            print!("decision ▸ ");
            if io::stdout().flush().is_err() {
                return Decision::Abort;
            }
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => return Decision::Abort, // EOF (Ctrl-D) — bail safely
                Ok(_) => {}
                Err(_) => return Decision::Abort,
            }
            match parse_decision(line.trim(), phase, &|prompt| read_line(&stdin, prompt)) {
                Some(d) => return d,
                None => {
                    eprintln!("  ? didn't understand that — try a, r, s, or x");
                    continue;
                }
            }
        }
    }
}

/// Read one trimmed line of input after printing `prompt`. Returns empty on EOF.
fn read_line(stdin: &io::Stdin, prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = stdin.read_line(&mut s);
    s.trim().to_string()
}

/// Parse a checkpoint decision keystroke into a [`sc_workflow::Decision`]. `ask` is
/// called for the follow-up prompts a decision needs (the send-back target phase and
/// its feedback note), so this stays pure and unit-testable — the I/O is injected.
/// Returns `None` for unrecognized input so the caller can re-prompt.
fn parse_decision(
    input: &str,
    current: sc_workflow::Phase,
    ask: &dyn Fn(&str) -> String,
) -> Option<sc_workflow::Decision> {
    use sc_workflow::{Decision, Phase};
    match input.to_ascii_lowercase().as_str() {
        "a" | "approve" | "" => Some(Decision::Approve),
        "r" | "revise" => Some(Decision::Revise),
        "x" | "abort" | "q" | "quit" => Some(Decision::Abort),
        "s" | "send-back" | "sendback" | "send" => {
            // Default target is the current phase (regenerate in place); the human
            // may name an earlier phase slug to bounce further back.
            let target_in = ask("  send back to which phase? (slug, blank = this phase) ▸ ");
            let target = if target_in.is_empty() {
                current
            } else {
                match Phase::from_slug(&target_in) {
                    Some(p) if p.index() <= current.index() => p,
                    Some(_) => {
                        eprintln!(
                            "  ! can only send back to this phase or earlier; using this phase"
                        );
                        current
                    }
                    None => {
                        eprintln!("  ! unknown phase {target_in:?}; using this phase");
                        current
                    }
                }
            };
            let notes = ask("  feedback for the regeneration (blank = none) ▸ ");
            Some(Decision::SendBack {
                target,
                notes: if notes.is_empty() { None } else { Some(notes) },
            })
        }
        _ => None,
    }
}

/// Drive a task with the worker swarm. By default this serves the live web
/// dashboard; `--cli` renders the swarm to the terminal (line-oriented), and
/// `--json` emits the `SwarmEvent` stream as NDJSON on stdout (spec 06 — "swarm
/// rendering"). `--json` implies the terminal path (the dashboard isn't headless).
fn swarm_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Preflight the backends before running — a crashed server otherwise looks
    // like silent worker failures on the dashboard / in the stream.
    let (orchestrator, worker, advisor) = (cli.orchestrator(), cli.backend(), cli.swarm_advisor());
    if let Err(e) = sc_cli::preflight(&[
        ("orchestrator", &orchestrator),
        ("worker", &worker),
        ("advisor", &advisor),
    ]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // `--cli` / `--json` drive the swarm directly and render its event stream to
    // the terminal, mirroring `run`'s TUI-vs-`run --json` split. The default
    // (neither flag) keeps the web dashboard.
    if cli.cli || cli.json {
        return swarm_task_cli(cli, task, &orchestrator, &worker, &advisor, &workspace);
    }

    // Workers use --base-url/--model; the orchestrator decomposes; advisor is the
    // optional senior. All three are OpenAI-compatible backends (spec 02/08).
    let spec = sc_web::WebSwarm {
        orchestrator: cli.orchestrator(),
        worker: cli.backend(),
        // Workers always get a senior to ask: the explicit --advisor if given,
        // else the orchestrator (already in VRAM). A stalled tiny worker that can
        // ask the bigger model how to proceed is the whole recovery story (spec 02).
        advisor: Some(cli.swarm_advisor()),
        task,
        repo_overview: String::new(),
        config: swarm_config_with_frozen(cli, &workspace),
        workspace,
    };

    let result = sc_web::serve_swarm(spec, "127.0.0.1:0", |url| {
        println!("smart-coder swarm dashboard live at {url}");
        println!("open it in your browser to watch the swarm (Ctrl-C to stop)");
    });

    match result {
        Ok(Some(report)) => {
            println!(
                "\nswarm: {} integrated, {} rejected, {} pending",
                report.done, report.failed, report.pending
            );
            if report.all_done {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: swarm server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build the swarm config, filling `frozen_paths` from the workspace when the user
/// didn't pass `--frozen` (spec 08/11). Freezing the test oracle enables the precise
/// per-subtask scoped completion check and stops a worker from rewriting a test to
/// make it "pass"; without it the swarm falls back to the coarse whole-suite-delta
/// check. An explicit `--frozen` list always wins.
fn swarm_config_with_frozen(cli: &Cli, workspace: &std::path::Path) -> sc_swarm::SwarmConfig {
    let mut cfg = cli.swarm_config();
    if cfg.frozen_paths.is_empty() {
        cfg.frozen_paths = sc_cli::detect_test_files(workspace);
    }
    cfg
}

/// Drive the swarm and render its event stream to the terminal — the line-oriented
/// counterpart of the web dashboard (spec 06 "swarm rendering (later)"). With
/// `--json` the stream is NDJSON on stdout (one `SwarmEvent` per line, re-parseable),
/// human notes to stderr; otherwise it's the readable task-board view (`--cli`).
fn swarm_task_cli(
    cli: &Cli,
    task: String,
    orchestrator: &sc_model::OpenAiBackend,
    worker: &sc_model::OpenAiBackend,
    advisor: &sc_model::OpenAiBackend,
    workspace: &std::path::Path,
) -> ExitCode {
    let cfg = swarm_config_with_frozen(cli, workspace);
    if cli.json {
        eprintln!("swarm: {task}");
    } else {
        println!("● swarm  {task}   (max {} workers)", cfg.max_workers);
    }

    // The sink renders each orchestrator event as it happens: JSON lines for
    // machines, the task-board view for humans.
    let report = if cli.json {
        let sink = JsonSwarmSink;
        sc_swarm::run_swarm(
            orchestrator,
            worker,
            Some(advisor as &(dyn sc_model::ModelBackend + Sync)),
            &task,
            "",
            workspace,
            &cfg,
            &sink,
        )
    } else {
        let sink = sc_swarm::FnSwarmSink(|e: &sc_swarm::SwarmEvent| print_swarm_event(e));
        sc_swarm::run_swarm(
            orchestrator,
            worker,
            Some(advisor as &(dyn sc_model::ModelBackend + Sync)),
            &task,
            "",
            workspace,
            &cfg,
            &sink,
        )
    };

    // Honest closing line (spec 06): the human-readable summary goes to stderr in
    // `--json` mode so it never pollutes the NDJSON a consumer is parsing.
    let summary = format!(
        "swarm: {} integrated, {} rejected, {} pending",
        report.done, report.failed, report.pending
    );
    if cli.json {
        eprintln!("{summary}");
    } else {
        println!("\n{summary}");
    }

    if report.all_done {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// A [`sc_swarm::SwarmSink`] that emits each event as one NDJSON line on stdout —
/// the swarm counterpart of [`sc_core::JsonLinesSink`], so `swarm --json` is
/// scriptable and the stream round-trips (`SwarmEvent` is Serialize↔Deserialize).
struct JsonSwarmSink;
impl sc_swarm::SwarmSink for JsonSwarmSink {
    fn record(&self, event: &sc_swarm::SwarmEvent) {
        match swarm_event_json(event) {
            Ok(line) => println!("{line}"),
            // Never let a serialization hiccup abort the swarm; note it on stderr.
            Err(e) => eprintln!("warning: could not serialize swarm event: {e}"),
        }
    }
}

/// Serialize one swarm event to a single NDJSON line (the body of [`JsonSwarmSink`],
/// split out so it's unit-testable without capturing stdout).
fn swarm_event_json(event: &sc_swarm::SwarmEvent) -> serde_json::Result<String> {
    serde_json::to_string(event)
}

/// Print one swarm event in the line-oriented style of spec 06 (mirrors
/// [`print_event`] for the per-worker stream): decomposition → which worker is on
/// which subtask → each integration accept/reject → the final tally.
fn print_swarm_event(ev: &sc_swarm::SwarmEvent) {
    use sc_swarm::SwarmEvent::*;
    match ev {
        Decomposed { subtasks } => {
            println!("● board  ({} subtasks)", subtasks.len());
            for (i, goal) in subtasks.iter().enumerate() {
                println!("  {}. {goal}", i + 1);
            }
        }
        OrchestratorPrompt { fell_back, .. } => {
            if *fell_back {
                println!(
                    "  ⚠ decomposition fell back to one subtask (orchestrator gave nothing usable)"
                );
            }
        }
        WorkerStarted { subtask, goal, .. } => {
            println!("▸ worker [{subtask}]  {goal}");
        }
        WorkerFinished {
            subtask, summary, ..
        } => {
            println!("  · [{subtask}] finished — {summary}");
        }
        SubtaskRetry {
            subtask,
            attempt,
            max,
            failing_tests,
        } => {
            let n = failing_tests.len();
            let plural = if n == 1 { "" } else { "s" };
            println!("  ↻ [{subtask}] retry {attempt}/{max} — {n} test{plural} still red");
        }
        AdvisorConsulted { subtask, advice } => {
            println!("  ⚑ [{subtask}] asked senior — {advice}");
        }
        Integrated {
            subtask,
            accepted,
            files,
        } => {
            if *accepted {
                let what = if files.is_empty() {
                    "(no file changes)".to_string()
                } else {
                    files.join(", ")
                };
                println!("  ✓ [{subtask}] integrated — {what}");
            } else {
                // On reject, `files[0]` carries the reason (spec / event.rs).
                let reason = files.first().map(String::as_str).unwrap_or("rejected");
                println!("  ✗ [{subtask}] reverted — {reason}");
            }
        }
        SwarmDone {
            done,
            failed,
            all_done,
        } => {
            let mark = if *all_done { "✔" } else { "■" };
            println!("{mark} swarm done — {done} integrated, {failed} failed");
        }
    }
}

/// Audit the current directory against a compliance framework pack and serve the
/// evidence pack as a local dashboard (spec 13).
///
/// No model backend is involved: the built-in collectors are deterministic, which
/// is the point — an evidence pack has to be reproducible and citable.
fn comply_task(cli: &Cli, pack_arg: Option<String>) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    // With --pack, show that one framework. Without, offer ALL shipped packs:
    // the interesting question is usually not "how do we score against SOC 2"
    // but "where do our frameworks overlap, and what is genuinely missing".
    //
    // Parse and validate before binding a port: a malformed pack must fail here,
    // not halfway through an audit whose output someone will sign.
    let frameworks = match build_frameworks(pack_arg) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let framework = frameworks
        .first()
        .map(|f| f.pack.framework.name.clone())
        .unwrap_or_default();
    let controls: usize = frameworks.iter().map(|f| f.pack.controls.len()).sum();
    let count = frameworks.len();
    let spec = sc_web::ComplyRun {
        workspace: workspace.clone(),
        frameworks,
        options: sc_comply::collector::ComplyOptions::default(),
    };

    // An empty token tells the server auth is off. mint_token() never returns
    // empty, so this can only happen via the explicit flag.
    let token = if cli.no_token {
        String::new()
    } else {
        sc_web::mint_token()
    };
    let addr = format!("127.0.0.1:{}", cli.port);
    let result = sc_web::serve_comply(spec, &addr, &token, |url| {
        if count > 1 {
            println!("sc-comply — {count} frameworks, {controls} controls total");
        } else {
            println!("sc-comply — {framework} ({controls} controls)");
        }
        println!("workspace: {}", workspace.display());
        if token.is_empty() {
            println!("evidence pack live at {url}/");
            println!("(--no-token: no URL secret. Bound to 127.0.0.1 only.)");
        } else {
            println!("evidence pack live at {url}/?k={token}");
        }
        println!("command checks are DISABLED by default; review the pack before enabling them.");
        if token.is_empty() {
            // Tailscale would expose an unauthenticated page to the tailnet.
            println!("do NOT `tailscale serve` this run — it has no token; restart without --no-token first.");
        } else {
            println!(
                "to reach it from your phone: run `tailscale serve {}` and open the",
                cli.port
            );
            println!("printed https URL with ?k={token} on the phone (same tailnet).");
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: compliance server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Critique a compliance pack's own authoring (spec 14).
///
/// Deterministic and model-free: this is the half of the authoring assistant
/// that needs no API key. It exits non-zero on a blocking finding so it can be
/// wired into a check gate.
fn comply_lint(pack_arg: Option<String>) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let pack = match resolve_pack(pack_arg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The current directory doubles as the sample workspace: the file-dependent
    // lints need real files to test globs and paths against.
    let sample = sc_comply_author::Sample::load(&workspace);
    let report = sc_comply_author::lint_pack(&pack, Some(&sample));

    print!("{}", sc_comply_author::report::markdown(&report));

    let blocking = report.blocking().len();
    if blocking > 0 {
        eprintln!("\n{blocking} blocking finding(s) — the pack needs work before use.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Run the compliance drafting eval across one or more models (spec 15).
///
/// Unlike every other subcommand here this one spends real tokens on purpose, so
/// it prints the call budget up front and reports progress per control — a
/// twelve-control run against a slow local model takes minutes.
fn comply_eval(cli: &Cli, model_specs: Vec<String>) -> ExitCode {
    if model_specs.is_empty() {
        eprintln!(
            "error: comply-eval needs at least one --author-model, e.g.\n  \
             --author-model gemini-pro-latest@https://generativelanguage.googleapis.com/v1beta/openai\n  \
             --author-model qwen3-coder-30b@http://localhost:11435/v1"
        );
        return ExitCode::FAILURE;
    }

    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let suite_path = workspace.join("crates/sc-comply-author/evals/controls.toml");
    let suite = match sc_comply_author::eval::EvalSuite::load(&suite_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The repo itself is the sample workspace, so glob-reachability lints have
    // real files to test against.
    let sample = sc_comply_author::Sample::load(&workspace);

    eprintln!(
        "compliance drafting eval — {} controls × {} model(s) = {}+ calls",
        suite.controls.len(),
        model_specs.len(),
        suite.controls.len() * model_specs.len()
    );

    let mut scores = Vec::new();
    for spec in &model_specs {
        let (model, url) = match spec.split_once('@') {
            Some((m, u)) => (m.to_string(), u.to_string()),
            None => (spec.clone(), cli.base_url.clone()),
        };

        // Deliberately NOT chaining with_detected_context(): it probes for
        // llama.cpp's meta.n_ctx, which a hosted provider does not serve, and
        // silently leaves the backend at the 8192 default.
        let mut backend = sc_model::OpenAiBackend::new(&url, &model).with_context_tokens(128_000);
        if let Some(k) = cli
            .api_key
            .clone()
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
        {
            if !k.trim().is_empty() {
                backend = backend.with_api_key(k);
            }
        }

        eprintln!("\n=== {model} ({url}) ===");
        let mut progress = |i: usize, n: usize, id: &str| {
            eprintln!("  [{i}/{n}] {id}");
        };
        match sc_comply_author::run_suite(&backend, &model, &suite, Some(&sample), &mut progress) {
            Ok(s) => {
                eprintln!(
                    "  -> {} dishonest, {:.0}%",
                    s.dishonest_count(),
                    s.total() * 100.0
                );
                scores.push(s);
            }
            Err(e) => {
                eprintln!("error: {model} failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    print!(
        "{}",
        if scores.len() > 1 {
            sc_comply_author::eval::report::comparison(&suite, &scores)
        } else {
            sc_comply_author::eval::report::markdown(&suite, &scores[0])
        }
    );

    // Any dishonest draft fails the run: that is the property being measured.
    let dishonest: usize = scores.iter().map(|s| s.dishonest_count()).sum();
    if dishonest > 0 {
        eprintln!("\n{dishonest} dishonest draft(s) across all models.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Audit every shipped framework and write a static, redacted HTML site.
///
/// Redaction happens once, here, immediately after each audit — the pack that
/// reaches the renderer has already had its citations removed, and the renderer
/// asserts that independently. Nothing downstream has to remember.
fn comply_export(out_arg: Option<String>) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let out_dir = workspace.join(out_arg.unwrap_or_else(|| "docs/compliance".to_string()));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("error: cannot create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let options = sc_comply::collector::ComplyOptions::default();
    let generated_at = sc_comply::evidence::now_rfc3339();
    let mut entries: Vec<sc_comply::report::site::IndexEntry> = Vec::new();

    eprintln!(
        "auditing {} frameworks -> {}",
        sc_comply::registry::SHIPPED.len(),
        out_dir.display()
    );

    for shipped in sc_comply::registry::SHIPPED {
        let pack = match sc_comply::registry::load_shipped(shipped.name) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let audited = match sc_comply::engine::audit_with(
            &workspace,
            &pack,
            &options,
            &sc_comply::collector::Registry::builtin(),
            generated_at.clone(),
        ) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("error: {} failed: {e}", shipped.name);
                return ExitCode::FAILURE;
            }
        };

        // Redact HERE, once. Everything downstream sees only the public pack.
        let public = audited.redacted();
        eprintln!(
            "  {:14} {} pass · {} gap · {} unknown",
            shipped.name, public.score.passed, public.score.gaps, public.score.unknown
        );

        let href = format!("{}.html", shipped.name);
        let html = sc_comply::report::site::framework_page(&public, Some("index.html"));
        if let Err(e) = std::fs::write(out_dir.join(&href), html) {
            eprintln!("error: writing {href}: {e}");
            return ExitCode::FAILURE;
        }
        entries.push(sc_comply::report::site::IndexEntry { href, pack: public });
    }

    // Cross-framework analysis, computed deterministically. This is what makes
    // an executive summary possible: a finding appearing in six of ten
    // frameworks is one fix with six times the leverage, and no per-framework
    // page can show that.
    let packs: Vec<sc_comply::evidence::EvidencePack> =
        entries.iter().map(|e| e.pack.clone()).collect();
    let rollup = sc_comply::rollup::roll_up(&packs);

    let project = workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this project".to_string());

    // The narrative is OPTIONAL. Without a configured model there is no
    // narrative and no error — the deterministic summary is complete on its own,
    // and most people running this will not have a key.
    let narrative = exec_narrative(&rollup, &project);

    let index =
        sc_comply::report::site::index_page(&entries, &project, &rollup, narrative.as_deref());
    if let Err(e) = std::fs::write(out_dir.join("index.html"), index) {
        eprintln!("error: writing index.html: {e}");
        return ExitCode::FAILURE;
    }

    let mut written = entries.len() + 1;

    // GitHub Pages serves from a directory ROOT, so when the output lands under
    // `docs/` that root needs its own landing page — without one a visitor gets
    // a 404 or a bare directory listing. Only written when the parent actually
    // looks like the docs tree, so `--out somewhere-else` does not scatter files.
    if let Some(site_root) = out_dir.parent() {
        if site_root.join("specs").is_dir() {
            let landing = sc_comply::report::site::landing_page(REPO_URL, &spec_links(site_root));
            match std::fs::write(site_root.join("index.html"), landing) {
                Ok(()) => {
                    written += 1;
                    println!("wrote landing page to {}", site_root.display());
                }
                Err(e) => eprintln!("warning: could not write the docs landing page: {e}"),
            }
            // Jekyll is disabled site-wide, so .nojekyll belongs at the SITE
            // root; a nested one has no effect. Move it if an older run left one.
            if let Err(e) = std::fs::write(site_root.join(".nojekyll"), "") {
                eprintln!("warning: could not write .nojekyll: {e}");
            }
            let _ = std::fs::remove_file(out_dir.join(".nojekyll"));
        } else if let Err(e) = std::fs::write(out_dir.join(".nojekyll"), "") {
            eprintln!("warning: could not write .nojekyll: {e}");
        }
    }

    println!("\nwrote {written} page(s) to {}", out_dir.display());
    println!("citations, file paths and excerpts are REDACTED from every page.");
    println!("review the output before committing, then enable GitHub Pages on docs/.");
    ExitCode::SUCCESS
}

/// Generate the executive summary, or `None` if no model is configured or the
/// output cannot be trusted.
///
/// Deliberately best-effort. An export that failed because a summary could not
/// be written would be a worse outcome than a page without one, and the page is
/// designed to stand alone. Every skip reason is printed so the operator knows
/// why the narrative is missing rather than wondering.
fn exec_narrative(rollup: &sc_comply::rollup::Rollup, project: &str) -> Option<String> {
    let key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())?;
    let model = std::env::var("SC_NARRATIVE_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "gemini-pro-latest".to_string());

    eprintln!("writing the executive summary with {model} ...");

    // NOT chaining with_detected_context(): it probes for llama.cpp's n_ctx,
    // which a hosted provider does not serve, silently capping context at 8192.
    let backend = sc_model::OpenAiBackend::new(GEMINI_OPENAI_URL, &model)
        .with_api_key(key)
        .with_context_tokens(128_000);

    let mut on_reject = |r: &sc_comply_author::narrative::Rejection| {
        eprintln!("  narrative rejected: {r} — publishing without it");
    };

    match sc_comply_author::narrative::generate(&backend, rollup, project, &mut on_reject) {
        Ok(Some(text)) => {
            eprintln!("  summary written ({} chars)", text.chars().count());
            Some(text)
        }
        Ok(None) => None,
        Err(e) => {
            eprintln!("  narrative unavailable ({e}) — publishing without it");
            None
        }
    }
}

/// Gemini's OpenAI-compatible endpoint.
const GEMINI_OPENAI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// The canonical repository URL, for links out of the published site.
const REPO_URL: &str = "https://github.com/jamez667/smart-coder";

/// Build the spec list for the landing page by reading `docs/specs/`.
///
/// Links point at GitHub rather than at relative `.md` paths: Jekyll is disabled
/// on this site, so a relative link would serve raw Markdown as a download.
///
/// Titles and summaries come from each file's own H1 and first prose line, so a
/// new spec appears on the site without anyone remembering to register it here.
fn spec_links(site_root: &std::path::Path) -> Vec<sc_comply::report::site::SpecLink> {
    let Ok(entries) = std::fs::read_dir(site_root.join("specs")) else {
        return Vec::new();
    };

    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();

    files
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let text = std::fs::read_to_string(path).ok()?;

            let title = text
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l.trim_start_matches("# ").trim().to_string())
                .unwrap_or_else(|| name.clone());

            // The first prose PARAGRAPH after the opening section heading — the
            // spec's own summary of itself. Joined across lines first, because
            // specs are hard-wrapped at ~80 columns and taking a single line
            // would cut most summaries mid-sentence.
            let para: String = text
                .lines()
                .skip_while(|l| !l.starts_with("## "))
                .skip(1)
                .skip_while(|l| l.trim().is_empty())
                .take_while(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .map(|l| l.trim_start_matches('>').trim())
                .collect::<Vec<_>>()
                .join(" ");
            let summary = first_sentence(para.trim());

            Some(sc_comply::report::site::SpecLink {
                title,
                href: format!("{REPO_URL}/blob/main/docs/specs/{name}"),
                summary,
            })
        })
        .collect()
}

/// The first sentence of a line, with Markdown emphasis stripped.
fn first_sentence(line: &str) -> String {
    let plain = line.replace("**", "").replace('`', "");
    match plain.find(". ") {
        Some(i) => plain[..=i].to_string(),
        None => plain,
    }
}

/// Build the framework list the dashboard offers.
///
/// With an explicit `--pack`, just that one. Without, every shipped pack — a
/// user who has not named a framework usually wants to see the landscape.
fn build_frameworks(pack_arg: Option<String>) -> Result<Vec<sc_web::FrameworkEntry>, String> {
    if let Some(spec) = pack_arg {
        let name = sc_comply::registry::find(&spec)
            .map(|p| p.name.to_string())
            .unwrap_or_else(|| {
                // A user-authored path: name it after the file so the selector
                // and the ?framework= query still have something to key on.
                std::path::Path::new(&spec)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "custom".to_string())
            });
        let pack = resolve_pack(Some(spec))?;
        return Ok(vec![sc_web::FrameworkEntry { name, pack }]);
    }

    let mut out = Vec::with_capacity(sc_comply::registry::SHIPPED.len());
    for entry in sc_comply::registry::SHIPPED {
        let pack = sc_comply::registry::load_shipped(entry.name).map_err(|e| e.to_string())?;
        out.push(sc_web::FrameworkEntry {
            name: entry.name.to_string(),
            pack,
        });
    }
    Ok(out)
}

/// Resolve `--pack` to a loaded pack.
///
/// Accepts a shipped pack NAME (`soc2`, `iso27001`, …) or a filesystem path to a
/// pack the user authored. Name first: the shipped packs are embedded, so a name
/// works from any directory against any workspace, whereas a path only works
/// relative to where the user happens to be standing.
///
/// With no argument, defaults to SOC 2 — the most widely requested framework and
/// a reasonable starting point for someone who has not yet chosen.
fn resolve_pack(arg: Option<String>) -> Result<sc_comply::pack::Pack, String> {
    let Some(spec) = arg else {
        return sc_comply::registry::load_shipped("soc2").map_err(|e| e.to_string());
    };

    if sc_comply::registry::find(&spec).is_some() {
        return sc_comply::registry::load_shipped(&spec).map_err(|e| e.to_string());
    }

    let path = std::path::PathBuf::from(&spec);
    if path.is_file() {
        return sc_comply::pack::Pack::load(&path).map_err(|e| e.to_string());
    }

    Err(format!(
        "{spec:?} is neither a shipped pack name nor a readable file.\n\n{}",
        sc_comply::registry::listing()
    ))
}

/// Serve the remote iterate server for the Android client: idle until a phone POSTs a
/// task to `/run`, then drives an in-place Iterate run over the current directory
/// (whatever project the PC has open). Reached via a Tailscale tunnel.
fn remote_task(cli: &Cli) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = sc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let strategy = sc_core::select_strategy(&backend.capabilities());

    let spec = sc_web::IterateServer {
        backend: std::sync::Arc::new(backend),
        advisor: cli.advisor().map(std::sync::Arc::new),
        registry: std::sync::Arc::new(sc_tools::default_registry()),
        strategy: strategy.into(),
        workspace: workspace.clone(),
        base_config: cli.agent_config(),
        configured_verify: cli.verify_command.clone(),
    };

    let token = sc_web::mint_token();
    let addr = format!("127.0.0.1:{}", cli.port);
    let ws_name = workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("(workspace)");
    let result = sc_web::serve_iterate(spec, &addr, &token, |url| {
        println!("smart-coder remote (iterate) live at {url}/?k={token}");
        println!("workspace: {ws_name}  ({})", workspace.display());
        println!("idle until a client POSTs a task to /run.");
        println!(
            "to reach it from your phone: run `tailscale serve {}` and open the",
            cli.port
        );
        println!("printed https URL with ?k={token} on the phone (same tailnet).");
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: remote server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Drive a task in the current directory and serve a live web dashboard.
fn serve_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = sc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());

    let spec = sc_web::WebRun {
        backend,
        advisor: cli.advisor(),
        registry,
        strategy,
        instruction: task,
        workspace,
        config: cli.agent_config(),
    };

    // Bind a fixed loopback port (never 0.0.0.0) and require a per-run token on every
    // request — defense-in-depth behind a Tailscale tunnel, since the dashboard can
    // approve/deny commands and cancel the run.
    let token = sc_web::mint_token();
    let addr = format!("127.0.0.1:{}", cli.port);
    let result = sc_web::serve(spec, &addr, &token, |url| {
        println!("smart-coder dashboard live at {url}/?k={token}");
        println!("open it in your browser to watch and drive the run (Ctrl-C to stop)");
        println!(
            "to reach it from your phone: run `tailscale serve {}` and open the",
            cli.port
        );
        println!("printed https URL with ?k={token} on the phone (same tailnet).");
    });

    match result {
        Ok(Some(report)) => {
            println!("\n{:?} — {}", report.stop_reason, report.change_summary);
            if report.finished {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: web server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Drive a coding task in the current directory with the live TUI.
fn run_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = sc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());

    // Every run is logged for later `replay` (spec 06). The TUI worker tees events
    // into this file alongside the live channel sink.
    let (log_path, session_id) = sc_cli::session_log_path(&workspace, cli.log.as_deref());

    let spec = sc_tui::TuiRun {
        backend,
        // "Junior asks senior" (spec 02): the optional larger advisor model.
        advisor: cli.advisor(),
        registry,
        strategy,
        instruction: task,
        workspace,
        config: cli.agent_config(),
        log: Some(log_path.clone()),
    };

    match sc_tui::run(spec) {
        Ok(Some(report)) => {
            // Honest stop line on the normal terminal after the TUI restores it.
            println!("{:?} — {}", report.stop_reason, report.change_summary);
            println!("session {session_id} logged to {}", log_path.display());
            if report.finished {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: TUI failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Drive a coding task headless, emitting the event stream as JSON lines on stdout
/// (`run --json`, spec 06). The same stream is teed to the session log so the run
/// is replayable. No TUI — this is the machine-readable / scriptable surface.
fn run_task_json(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = sc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let advisor = cli.advisor();
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
    let config = cli.agent_config();

    // stdout: the machine-readable JSON-lines stream.
    let stdout_sink = sc_core::JsonLinesSink::new(io::stdout().lock());

    // log file: the same stream, persisted for `replay`. A failure to open the log
    // is a warning, not fatal — the run (and its stdout stream) still proceed.
    let (log_path, session_id) = sc_cli::session_log_path(&workspace, cli.log.as_deref());
    let log_file = open_log(&log_path);
    let log_sink = log_file.map(sc_core::JsonLinesSink::new);

    let mut sinks: Vec<&dyn sc_core::EventSink> = vec![&stdout_sink];
    if let Some(ref s) = log_sink {
        sinks.push(s);
    }
    let tee = sc_core::TeeSink::new(sinks);

    let result = sc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn sc_model::ModelBackend),
        &registry,
        strategy.as_ref(),
        &task,
        &workspace,
        &config,
        &tee,
    );

    match result {
        Ok(report) => {
            // The structured stream is on stdout; the human note goes to stderr so
            // it never pollutes the JSON a consumer is parsing.
            if log_sink.is_some() {
                eprintln!("session {session_id} logged to {}", log_path.display());
            }
            if report.finished {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("error: run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `staged <task>` — the headless staged-decomposition BUILD. Unlike `run` (one agent
/// loop over the whole task), this first runs the plan-only workflow to a stage
/// breakdown, then lands each scoped stage with `staged_build`, gated by a per-stage
/// verify (default `cargo check --workspace`, overridable with `--verify`). Emits the
/// same JSON-lines `AgentEvent` stream on stdout as `run --json` (staged_build takes the
/// same `EventSink`); phase and stage boundaries are logged to stderr to keep stdout pure
/// NDJSON.
fn staged_task_json(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = sc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // Phase 1 — plan only, to a stage breakdown. The orchestrator and worker are the same
    // backend here (one model); phases log to stderr so stdout stays pure NDJSON.
    let on_phase = |p: sc_workflow::Phase, _content: &str| eprintln!("phase: {}", p.title());
    let outcome = match sc_workflow::run_workflow_moded(
        &backend,
        &backend,
        &task,
        &workspace,
        sc_workflow::ThinkPolicy::default(),
        sc_workflow::WorkflowMode::plan_only(),
        &on_phase,
        &sc_workflow::AutoApprove,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: planning failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let breakdown = outcome
        .state
        .artifact(sc_workflow::Phase::StageBreakdown)
        .map(|a| a.content.clone())
        .unwrap_or_default();
    let stages = sc_workflow::parse_stages(&breakdown);
    if stages.is_empty() {
        eprintln!("error: stage breakdown produced no stages; nothing to build");
        return ExitCode::FAILURE;
    }
    eprintln!("planned {} stage(s)", stages.len());

    // Phase 2 — build each stage, gated by the per-stage verify. Same NDJSON sinks as
    // run_task_json: stdout stream + optional persisted log tee.
    let stdout_sink = sc_core::JsonLinesSink::new(io::stdout().lock());
    let (log_path, session_id) = sc_cli::session_log_path(&workspace, cli.log.as_deref());
    let log_file = open_log(&log_path);
    let log_sink = log_file.map(sc_core::JsonLinesSink::new);
    let mut sinks: Vec<&dyn sc_core::EventSink> = vec![&stdout_sink];
    if let Some(ref s) = log_sink {
        sinks.push(s);
    }
    let tee = sc_core::TeeSink::new(sinks);

    // The per-stage gate. Rust default; overridable via --verify for other stacks.
    let verify = cli
        .verify_command
        .clone()
        .unwrap_or_else(|| "cargo check --workspace".to_string());
    let on_stage = |i: usize, s: &sc_workflow::Stage| eprintln!("stage {i}: {}", s.title);

    let report = match sc_workflow::staged_build(
        &backend,
        &stages,
        &workspace,
        &verify,
        None, // no behavioral oracle for the headless build; the caller verifies on the host
        &cli.agent_config(),
        &on_stage,
        &tee,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: staged build failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if log_sink.is_some() {
        eprintln!("session {session_id} logged to {}", log_path.display());
    }
    if report.verified {
        ExitCode::SUCCESS
    } else {
        eprintln!("staged build did not reach a verified state");
        ExitCode::FAILURE
    }
}

/// Re-render a recorded session (`replay <id>`, spec 06): read the JSON-lines log,
/// deserialize each event, and print it with the same line-oriented formatter used
/// live. A bare id resolves under `.smart-coder/sessions/`; a path is used directly.
fn replay(session: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = sc_cli::resolve_replay_path(&workspace, &session);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "error: cannot read session log {}: {e}\n  \
                 (looked for a session id under .smart-coder/sessions/ or a path)",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    println!("▶ replay {} ({})\n", session, path.display());
    let mut n = 0usize;
    let mut bad = 0usize;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<sc_core::AgentEvent>(line) {
            Ok(ev) => {
                print_event(&ev);
                n += 1;
            }
            Err(e) => {
                eprintln!("  ! line {}: not a valid event ({e})", i + 1);
                bad += 1;
            }
        }
    }
    println!(
        "\n— end of replay: {n} events{}",
        if bad > 0 {
            format!(", {bad} unreadable")
        } else {
            String::new()
        }
    );
    ExitCode::SUCCESS
}

/// Print one event in the line-oriented style of spec 06 (the static surface for
/// replay; the live TUI/web renderers consume the same events differently).
fn print_event(ev: &sc_core::AgentEvent) {
    use sc_core::AgentEvent::*;
    match ev {
        RunStarted {
            task,
            prompt_budget,
        } => {
            println!("● run  {task}   (budget {prompt_budget} tok)");
        }
        Planned { steps } => {
            println!("● plan");
            for (i, s) in steps.iter().enumerate() {
                println!("  {}. {s}", i + 1);
            }
        }
        PlanRevised { steps } => {
            println!("● plan revised");
            for (i, s) in steps.iter().enumerate() {
                println!("  {}. {s}", i + 1);
            }
        }
        PromptAssembled {
            step,
            tokens,
            messages,
        } => {
            // Verbose: the full prompt the model saw (spec 06). Print every message
            // verbatim so replay reproduces exactly what was sent.
            println!("⌖ prompt[{step}]  ({} msgs, {tokens} tok)", messages.len());
            for m in messages {
                println!("  ┌─ {} ─────────", m.role);
                for line in m.content.lines() {
                    println!("  │ {line}");
                }
            }
        }
        ModelTurn {
            step,
            prompt_tokens,
            ..
        } => {
            println!("· turn {step}   ({prompt_tokens} tok)");
        }
        ToolCall { tool, arg } => {
            println!("▸ {tool}  {arg}");
        }
        ToolResult {
            summary, is_error, ..
        } => {
            let mark = if *is_error { "✗" } else { "└" };
            println!("  {mark} {summary}");
        }
        RepairTriggered { detail } => {
            println!("  ↻ repair: {detail}");
        }
        Verification { green, summary, .. } => {
            println!("▸ verify  {} {summary}", if *green { "✓" } else { "✗" });
        }
        Stalled { trigger } => {
            println!("  ⚠ stalled: {trigger}");
        }
        Advice { trigger, advice } => {
            println!("  ☎ advisor ({trigger}): {advice}");
        }
        Diagnosis { trigger, report } => {
            println!("  🔬 diagnosis ({trigger}): {report}");
        }
        Stopped { reason } => {
            println!("■ stopped — {reason:?}");
        }
        ConfirmPending { id, command, .. } => {
            println!("  ⏸ approval needed [{id}]: {command}");
        }
        ConfirmResolved { id, allowed } => {
            println!("  {} resolved [{id}]", if *allowed { "✓" } else { "✗" });
        }
        ChatMessage { role, text } => {
            println!("  💬 {role}: {text}");
        }
        // The streaming increments aren't discrete CLI lines (the full text arrives at end).
        ContentDelta { .. } | ChatDelta { .. } => {}
    }
}

/// Open (create/truncate) a session log file, creating the parent dir. Returns
/// `None` on failure (logging is best-effort — never break a run over it).
fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("warning: cannot create log dir {}: {e}", parent.display());
            return None;
        }
    }
    match std::fs::File::create(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("warning: cannot open log {}: {e}", path.display());
            None
        }
    }
}

fn run_doctor(cli: &Cli) -> ExitCode {
    let backend = cli.backend();
    let reachable = probe(&backend);
    let ok = reachable.is_ok();
    println!(
        "{}",
        doctor_report(cli, &backend.capabilities(), &reachable)
    );
    if ok {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "\nThe backend isn't serving the model. Is the server running and the \
             model pulled?\n  e.g.  ollama serve   &&   ollama pull {}",
            cli.model
        );
        ExitCode::FAILURE
    }
}

/// A trivial multi-turn chat REPL: read a line, generate, print, repeat. History
/// is carried so follow-ups have context (spec 06). No tools — that's M1+.
fn run_chat(cli: &Cli) -> ExitCode {
    let backend = cli.backend();
    println!(
        "smart-coder chat — {} via {} (Ctrl-D or `exit` to quit)\n",
        cli.model, cli.base_url
    );

    let mut history = vec![Message::system(
        "You are smart-coder, a concise terminal coding assistant.",
    )];
    let stdin = io::stdin();

    loop {
        print!("you ▸ ");
        if io::stdout().flush().is_err() {
            return ExitCode::FAILURE;
        }

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                return ExitCode::SUCCESS; // EOF (Ctrl-D)
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("input error: {e}");
                return ExitCode::FAILURE;
            }
        }

        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "exit" | "quit") {
            return ExitCode::SUCCESS;
        }

        history.push(Message::user(prompt.to_string()));
        let req = GenerateRequest::new(history.clone());
        match backend.generate(&req) {
            Ok(resp) => {
                println!("dc  ▸ {}\n", resp.content.trim());
                history.push(Message::assistant(resp.content));
            }
            Err(e) => {
                // Don't poison the history with a failed turn; let the user retry.
                history.pop();
                eprintln!("error: {e}\n");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_workflow::{Decision, Phase};

    /// An `ask` stub that hands back a fixed sequence of answers for the follow-up
    /// prompts (send-back target, then feedback note).
    fn answers(seq: &[&str]) -> impl Fn(&str) -> String {
        let seq: Vec<String> = seq.iter().map(|s| s.to_string()).collect();
        let i = std::cell::Cell::new(0);
        move |_prompt: &str| {
            let n = i.get();
            i.set(n + 1);
            seq.get(n).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn approve_revise_abort_keystrokes() {
        let no_ask = |_: &str| String::new();
        // Approve: explicit, long form, and the empty default all approve.
        for k in ["a", "approve", ""] {
            assert_eq!(
                parse_decision(k, Phase::Specs, &no_ask),
                Some(Decision::Approve)
            );
        }
        assert_eq!(
            parse_decision("r", Phase::Specs, &no_ask),
            Some(Decision::Revise)
        );
        for k in ["x", "abort", "q"] {
            assert_eq!(
                parse_decision(k, Phase::Specs, &no_ask),
                Some(Decision::Abort)
            );
        }
        // Garbage re-prompts (None).
        assert_eq!(parse_decision("huh", Phase::Specs, &no_ask), None);
    }

    #[test]
    fn send_back_defaults_to_current_phase_with_no_notes() {
        // Blank target → this phase; blank notes → None.
        let ask = answers(&["", ""]);
        assert_eq!(
            parse_decision("s", Phase::Layout, &ask),
            Some(Decision::SendBack {
                target: Phase::Layout,
                notes: None,
            })
        );
    }

    #[test]
    fn send_back_targets_an_earlier_phase_with_notes() {
        let ask = answers(&["architecture", "make it event-driven"]);
        assert_eq!(
            parse_decision("s", Phase::Layout, &ask),
            Some(Decision::SendBack {
                target: Phase::Architecture,
                notes: Some("make it event-driven".to_string()),
            })
        );
    }

    #[test]
    fn send_back_to_a_later_phase_is_clamped_to_current() {
        // You can't bounce forward — naming a downstream phase falls back to here.
        let ask = answers(&["work-decomposition", ""]);
        assert_eq!(
            parse_decision("s", Phase::Layout, &ask),
            Some(Decision::SendBack {
                target: Phase::Layout,
                notes: None,
            })
        );
    }

    #[test]
    fn send_back_unknown_phase_falls_back_to_current() {
        let ask = answers(&["nonsense", ""]);
        assert_eq!(
            parse_decision("s", Phase::Architecture, &ask),
            Some(Decision::SendBack {
                target: Phase::Architecture,
                notes: None,
            })
        );
    }

    /// Every swarm event variant the CLI renders. Kept in sync with the renderer
    /// (`print_swarm_event`) and the JSON sink so both are exercised over the full
    /// set, not just the happy path.
    fn all_swarm_events() -> Vec<sc_swarm::SwarmEvent> {
        use sc_swarm::SwarmEvent::*;
        vec![
            Decomposed {
                subtasks: vec!["add validation".into(), "add a test".into()],
            },
            WorkerStarted {
                subtask: "s1".into(),
                goal: "add validation".into(),
                prompt: "Task: add validation\n…".into(),
            },
            WorkerFinished {
                subtask: "s1".into(),
                summary: "edited config.py".into(),
                proposal: "proposed body".into(),
            },
            SubtaskRetry {
                subtask: "s1".into(),
                attempt: 1,
                max: 2,
                failing_tests: vec!["test_upper_bound".into()],
            },
            AdvisorConsulted {
                subtask: "s1".into(),
                advice: "also clamp the upper bound".into(),
            },
            Integrated {
                subtask: "s1".into(),
                accepted: true,
                files: vec!["config.py".into()],
            },
            // Accepted with no file changes — the empty-files branch.
            Integrated {
                subtask: "s2".into(),
                accepted: true,
                files: vec![],
            },
            // Rejected — files[0] is the reason.
            Integrated {
                subtask: "s3".into(),
                accepted: false,
                files: vec!["suite went red".into()],
            },
            SwarmDone {
                done: 2,
                failed: 1,
                all_done: false,
            },
        ]
    }

    #[test]
    fn swarm_renderer_handles_every_variant() {
        // The line renderer must not panic on any variant (incl. the empty-files
        // and rejected branches). Output goes to stdout; we only assert no panic.
        for ev in all_swarm_events() {
            print_swarm_event(&ev);
        }
    }

    #[test]
    fn json_sink_lines_round_trip() {
        // The `--json` swarm surface must emit one re-parseable NDJSON line per
        // event (parity with `run --json`): serialize, then deserialize back.
        for ev in all_swarm_events() {
            let line = swarm_event_json(&ev).expect("serialize");
            assert!(
                !line.contains('\n'),
                "NDJSON line must be single-line: {line}"
            );
            let back: sc_swarm::SwarmEvent = serde_json::from_str(&line).expect("deserialize back");
            assert_eq!(back, ev, "round-trip mismatch for {line}");
        }
    }
}
