//! `dumb-coder` binary — a thin I/O shell over [`dc_cli`] (spec 06, M0).
//!
//! Parses args, then either prints the `doctor` report or runs a line-oriented
//! chat REPL. All the testable logic is in the library; this file is just stdin/
//! stdout plumbing.

use std::io::{self, Write};
use std::process::ExitCode;

use dc_cli::{doctor_report, probe, usage, Cli, Command};
use dc_model::{GenerateRequest, Message, ModelBackend};

fn main() -> ExitCode {
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
        Command::Swarm { task } => swarm_task(&cli, task.clone()),
        Command::Plan { task, interactive } => plan_task(&cli, task.clone(), *interactive),
        Command::Replay { session } => replay(session.clone()),
    }
}

/// Run the staged planning workflow (spec 09): the orchestrator (T1) plans each
/// phase, workers (T2) write the tests from the Phase-4 coverage plan, and — when a
/// `--verify` command is given — the swarm implements the work decomposition against
/// those tests until the suite is green. Plan artifacts land in `.dumb-coder/plan/`.
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
    if let Err(e) = dc_cli::preflight(&[("orchestrator", &orchestrator), ("worker", &worker)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let on_phase = |phase: dc_workflow::Phase, content: &str| {
        let preview: String = content.lines().take(8).collect::<Vec<_>>().join("\n");
        println!("\n=== {} ===\n{preview}\n…", phase.title());
    };

    // Autonomous by default; `--interactive`/`--gate`/`--ceremony`/`--gates` put a
    // human at the gates (spec 09). Adaptive ceremony scales *which* phases stop:
    // the resolved gate set decides which phases consult the stdin gate; the rest
    // auto-approve. The gate is fully harness-owned.
    let auto = dc_workflow::AutoApprove;
    let stdin_gate = StdinGate::new(&workspace);
    let gate_set = cli.ceremony_gates();
    let ceremony_gate = dc_workflow::CeremonyGate::new(gate_set, &stdin_gate);
    let gated = cli.plan_is_gated(interactive);
    let gate: &dyn dc_workflow::Gate = if gated { &ceremony_gate } else { &auto };
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

    let outcome = match dc_workflow::run_workflow_gated(
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
        println!("\nplan aborted at a checkpoint — approved artifacts kept in .dumb-coder/plan/");
        return ExitCode::SUCCESS;
    }

    println!(
        "\nplan complete — 6 phase artifacts in .dumb-coder/plan/\n  tests written: {}\n  subtasks for the swarm: {}",
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
    let sink = dc_swarm::NullSwarmSink;
    let report = dc_swarm::run_swarm_board(
        &orchestrator,
        &worker,
        Some(&advisor as &(dyn dc_model::ModelBackend + Sync)),
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

impl dc_workflow::Gate for StdinGate {
    fn decide(
        &self,
        phase: dc_workflow::Phase,
        artifact: &dc_workflow::Artifact,
    ) -> dc_workflow::Decision {
        use dc_workflow::Decision;
        let file = dc_workflow::plan_dir(&self.workspace).join(phase.filename());
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

/// Parse a checkpoint decision keystroke into a [`dc_workflow::Decision`]. `ask` is
/// called for the follow-up prompts a decision needs (the send-back target phase and
/// its feedback note), so this stays pure and unit-testable — the I/O is injected.
/// Returns `None` for unrecognized input so the caller can re-prompt.
fn parse_decision(
    input: &str,
    current: dc_workflow::Phase,
    ask: &dyn Fn(&str) -> String,
) -> Option<dc_workflow::Decision> {
    use dc_workflow::{Decision, Phase};
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
    if let Err(e) = dc_cli::preflight(&[
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
    let spec = dc_web::WebSwarm {
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

    let result = dc_web::serve_swarm(spec, "127.0.0.1:0", |url| {
        println!("dumb-coder swarm dashboard live at {url}");
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
fn swarm_config_with_frozen(cli: &Cli, workspace: &std::path::Path) -> dc_swarm::SwarmConfig {
    let mut cfg = cli.swarm_config();
    if cfg.frozen_paths.is_empty() {
        cfg.frozen_paths = dc_cli::detect_test_files(workspace);
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
    orchestrator: &dc_model::OpenAiBackend,
    worker: &dc_model::OpenAiBackend,
    advisor: &dc_model::OpenAiBackend,
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
        dc_swarm::run_swarm(
            orchestrator,
            worker,
            Some(advisor as &(dyn dc_model::ModelBackend + Sync)),
            &task,
            "",
            workspace,
            &cfg,
            &sink,
        )
    } else {
        let sink = dc_swarm::FnSwarmSink(|e: &dc_swarm::SwarmEvent| print_swarm_event(e));
        dc_swarm::run_swarm(
            orchestrator,
            worker,
            Some(advisor as &(dyn dc_model::ModelBackend + Sync)),
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

/// A [`dc_swarm::SwarmSink`] that emits each event as one NDJSON line on stdout —
/// the swarm counterpart of [`dc_core::JsonLinesSink`], so `swarm --json` is
/// scriptable and the stream round-trips (`SwarmEvent` is Serialize↔Deserialize).
struct JsonSwarmSink;
impl dc_swarm::SwarmSink for JsonSwarmSink {
    fn record(&self, event: &dc_swarm::SwarmEvent) {
        match swarm_event_json(event) {
            Ok(line) => println!("{line}"),
            // Never let a serialization hiccup abort the swarm; note it on stderr.
            Err(e) => eprintln!("warning: could not serialize swarm event: {e}"),
        }
    }
}

/// Serialize one swarm event to a single NDJSON line (the body of [`JsonSwarmSink`],
/// split out so it's unit-testable without capturing stdout).
fn swarm_event_json(event: &dc_swarm::SwarmEvent) -> serde_json::Result<String> {
    serde_json::to_string(event)
}

/// Print one swarm event in the line-oriented style of spec 06 (mirrors
/// [`print_event`] for the per-worker stream): decomposition → which worker is on
/// which subtask → each integration accept/reject → the final tally.
fn print_swarm_event(ev: &dc_swarm::SwarmEvent) {
    use dc_swarm::SwarmEvent::*;
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
    if let Err(e) = dc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());

    let spec = dc_web::WebRun {
        backend,
        advisor: cli.advisor(),
        registry,
        strategy,
        instruction: task,
        workspace,
        config: cli.agent_config(),
    };

    // Bind a localhost port (0 = OS-assigned) and print the URL to open.
    let result = dc_web::serve(spec, "127.0.0.1:0", |url| {
        println!("dumb-coder dashboard live at {url}");
        println!("open it in your browser to watch the run (Ctrl-C to stop)");
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
    if let Err(e) = dc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());

    // Every run is logged for later `replay` (spec 06). The TUI worker tees events
    // into this file alongside the live channel sink.
    let (log_path, session_id) = dc_cli::session_log_path(&workspace, cli.log.as_deref());

    let spec = dc_tui::TuiRun {
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

    match dc_tui::run(spec) {
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
    if let Err(e) = dc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let advisor = cli.advisor();
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());
    let config = cli.agent_config();

    // stdout: the machine-readable JSON-lines stream.
    let stdout_sink = dc_core::JsonLinesSink::new(io::stdout().lock());

    // log file: the same stream, persisted for `replay`. A failure to open the log
    // is a warning, not fatal — the run (and its stdout stream) still proceed.
    let (log_path, session_id) = dc_cli::session_log_path(&workspace, cli.log.as_deref());
    let log_file = open_log(&log_path);
    let log_sink = log_file.map(dc_core::JsonLinesSink::new);

    let mut sinks: Vec<&dyn dc_core::EventSink> = vec![&stdout_sink];
    if let Some(ref s) = log_sink {
        sinks.push(s);
    }
    let tee = dc_core::TeeSink::new(sinks);

    let result = dc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn dc_model::ModelBackend),
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

/// Re-render a recorded session (`replay <id>`, spec 06): read the JSON-lines log,
/// deserialize each event, and print it with the same line-oriented formatter used
/// live. A bare id resolves under `.dumb-coder/sessions/`; a path is used directly.
fn replay(session: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = dc_cli::resolve_replay_path(&workspace, &session);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "error: cannot read session log {}: {e}\n  \
                 (looked for a session id under .dumb-coder/sessions/ or a path)",
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
        match serde_json::from_str::<dc_core::AgentEvent>(line) {
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
fn print_event(ev: &dc_core::AgentEvent) {
    use dc_core::AgentEvent::*;
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
        "dumb-coder chat — {} via {} (Ctrl-D or `exit` to quit)\n",
        cli.model, cli.base_url
    );

    let mut history = vec![Message::system(
        "You are dumb-coder, a concise terminal coding assistant.",
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
    use dc_workflow::{Decision, Phase};

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
    fn all_swarm_events() -> Vec<dc_swarm::SwarmEvent> {
        use dc_swarm::SwarmEvent::*;
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
            let back: dc_swarm::SwarmEvent = serde_json::from_str(&line).expect("deserialize back");
            assert_eq!(back, ev, "round-trip mismatch for {line}");
        }
    }
}
