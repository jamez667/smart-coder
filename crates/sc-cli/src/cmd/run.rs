//! The task-running subcommands: `run` (TUI), `run --json` (headless NDJSON),
//! `serve` (web dashboard), `staged` (plan-then-build), and `remote` (the phone's
//! iterate server).

use std::io;
use std::process::ExitCode;

use sc_cli::Cli;
use sc_model::ModelBackend;

use super::common::{open_log, workspace};

/// Serve the remote iterate server for the Android client: idle until a phone POSTs a
/// task to `/run`, then drives an in-place Iterate run over the current directory
/// (whatever project the PC has open). Reached via a Tailscale tunnel.
pub fn remote_task(cli: &Cli) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
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
pub fn serve_task(cli: &Cli, task: String) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
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
pub fn run_task(cli: &Cli, task: String) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
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
pub fn run_task_json(cli: &Cli, task: String) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
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
pub fn staged_task_json(cli: &Cli, task: String) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
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
