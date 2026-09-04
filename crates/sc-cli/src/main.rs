//! `smart-coder` binary — a thin I/O shell over [`sc_cli`] (spec 06, M0).
//!
//! Parses args, then dispatches to one of the [`cmd`] modules. All the testable
//! parsing/config logic is in the library; the subcommand modules are the stdin/
//! stdout plumbing around it.

mod cmd;

use std::process::ExitCode;

use sc_cli::{usage, Cli, Command};

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
        Command::Doctor => cmd::chat::run_doctor(&cli),
        Command::Chat => cmd::chat::run_chat(&cli),
        Command::Run { task } if cli.json => cmd::run::run_task_json(&cli, task.clone()),
        Command::Run { task } => cmd::run::run_task(&cli, task.clone()),
        Command::Serve { task } => cmd::run::serve_task(&cli, task.clone()),
        Command::Remote => cmd::run::remote_task(&cli),
        Command::Comply { pack } => cmd::comply::comply_task(&cli, pack.clone()),
        Command::ComplyLint { pack } => cmd::comply::comply_lint(pack.clone()),
        Command::ListPacks => {
            print!("{}", sc_comply::registry::listing());
            ExitCode::SUCCESS
        }
        Command::ComplyEval { models } => cmd::comply::comply_eval(&cli, models.clone()),
        Command::ComplyExport { out } => cmd::comply::comply_export(out.clone()),
        Command::Swarm { task } => cmd::swarm::swarm_task(&cli, task.clone()),
        Command::Plan { task, interactive } => {
            cmd::plan::plan_task(&cli, task.clone(), *interactive)
        }
        Command::Staged { task } => cmd::run::staged_task_json(&cli, task.clone()),
        Command::Trace { check } => cmd::trace::trace(cli.json, *check),
        Command::Index => cmd::index::index(cli.json),
        Command::Search { query } => cmd::index::search(query, cli.json),
        Command::Health => cmd::index::health(cli.json),
        Command::Stack => cmd::index::stack(cli.json),
        Command::Queue { action } => cmd::queue::queue(&cli, action),
        Command::Replay { session } => cmd::replay::replay(session.clone()),
    }
}
