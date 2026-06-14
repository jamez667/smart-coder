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
        Command::Run { task } => run_task(&cli, task.clone()),
        Command::Serve { task } => serve_task(&cli, task.clone()),
        Command::Swarm { task } => swarm_task(&cli, task.clone()),
        Command::Plan { task } => plan_task(&cli, task.clone()),
    }
}

/// Run the staged planning workflow (spec 09) autonomously: the orchestrator (T1)
/// plans each phase, workers (T2) write the tests from the Phase-4 coverage plan,
/// and — when a `--verify` command is given — the swarm implements the work
/// decomposition against those tests until the suite is green. Plan artifacts land
/// in `.dumb-coder/plan/`.
fn plan_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let orchestrator = cli.orchestrator();
    let worker = cli.backend();
    let on_phase = |phase: dc_workflow::Phase, content: &str| {
        let preview: String = content.lines().take(8).collect::<Vec<_>>().join("\n");
        println!("\n=== {} ===\n{preview}\n…", phase.title());
    };

    let outcome = match dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        cli.think_policy(),
        &on_phase,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: workflow failed: {e}");
            return ExitCode::FAILURE;
        }
    };

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

/// Drive a task with the worker swarm and serve the swarm dashboard.
fn swarm_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

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
        workspace,
        config: cli.swarm_config(),
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
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());

    let spec = dc_tui::TuiRun {
        backend,
        // "Junior asks senior" (spec 02): the optional larger advisor model.
        advisor: cli.advisor(),
        registry,
        strategy,
        instruction: task,
        workspace,
        config: cli.agent_config(),
    };

    match dc_tui::run(spec) {
        Ok(Some(report)) => {
            // Honest stop line on the normal terminal after the TUI restores it.
            println!("{:?} — {}", report.stop_reason, report.change_summary);
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
