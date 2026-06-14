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
        advisor: Option::<dc_model::OpenAiBackend>::None,
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
        // No separate advisor profile yet (M4 escalation is wired but the CLI
        // doesn't expose a second model); the senior is None for now.
        advisor: Option::<dc_model::OpenAiBackend>::None,
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
