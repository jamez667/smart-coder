//! `sc-eval` binary — score a solver on a task suite, print a report.
//!
//! Two solvers:
//!
//!     sc-eval [SUITE_TOML]              the red->green demo suite (FileSolver)
//!     sc-eval --agent [SUITE_TOML] [--url <U>] [--model <M>]   the same suite, real model
//!             [--only <ID>] [--repeat <N>] [--log <DIR>]      one task, N times, streamed

use std::process::ExitCode;

use sc_eval::{run_suite, AgentSolver, FileSolver, Report, TaskSuite};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--agent") {
        return agent_suite(&args);
    }
    demo_suite(&args)
}

/// The original behaviour, unchanged: prove the harness end to end with a solver that
/// applies each task's known solution.
fn demo_suite(args: &[String]) -> ExitCode {
    let suite_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "evals/suite.toml".to_string());

    println!("smart-coder eval harness\n  solver: demo FileSolver\n  suite: {suite_path}\n");

    let suite = match TaskSuite::load(std::path::Path::new(&suite_path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let report = Report::new(run_suite(&suite.tasks, &FileSolver));
    println!("{}", report.summary());

    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).map(String::as_str)
}

/// Score a REAL MODEL on the fixed task suite.
///
/// This entry point did not exist, and its absence hid a bug: `AgentSolver` was
/// reachable from no binary at all, so nothing ever ran the task path against a
/// model. `AgentSolver::with_config` sat unused and task runs quietly used
/// `AgentConfig::default()` -- values tuned for toy tasks -- while every measurement
/// was being made on the SWE-bench path instead. A path with no entry point does not
/// get exercised, and a path that does not get exercised rots.
fn agent_suite(args: &[String]) -> ExitCode {
    let url = flag(args, "--url").unwrap_or("http://localhost:11436/v1");
    let model = flag(args, "--model").unwrap_or("tiel-coder-35b");
    let suite_path = args
        .iter()
        .find(|a| !a.starts_with("--") && a.ends_with(".toml"))
        .cloned()
        .unwrap_or_else(|| "evals/suite.toml".to_string());

    println!(
        "smart-coder eval harness\n  solver: agent ({model} @ {url})\n  suite: {suite_path}\n"
    );

    let suite = match TaskSuite::load(std::path::Path::new(&suite_path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // `--only <ID>` narrows to one task, and `--repeat <N>` runs the selection N
    // times. Together they are how you investigate a rung that passes on one run
    // and fails the next: the same task, several times, with the stream captured.
    let only = flag(args, "--only");
    let repeat: usize = flag(args, "--repeat")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let tasks: Vec<_> = match only {
        Some(id) => suite.tasks.iter().filter(|t| t.id == id).cloned().collect(),
        None => suite.tasks.clone(),
    };
    if tasks.is_empty() {
        eprintln!("error: no task matched --only {}", only.unwrap_or(""));
        return ExitCode::FAILURE;
    }

    // `with_detected_context` adopts the server's real n_ctx. Without it the backend
    // assumes 8192, and since `task_config` reserves 12288 tokens for the reply,
    // `prompt_budget` saturates to ZERO -- no ceiling, nothing evicted, and the
    // prompt grows until the SERVER rejects it. That surfaced as
    // "request (33164 tokens) exceeds the available context size (32768)", which
    // reads as a model problem and is entirely ours.
    let backend = sc_model::OpenAiBackend::new(url, model)
        .with_detected_context()
        .with_native_tools();
    let mut all_passed = true;
    for round in 1..=repeat {
        if repeat > 1 {
            println!("--- run {round}/{repeat} ---");
        }
        // One log file per run, so runs of the same task stay separable.
        let log = flag(args, "--log").map(|dir| {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = std::fs::create_dir_all(dir);
            std::path::PathBuf::from(dir).join(format!("run-{stamp}-{round}.ndjson"))
        });
        let file = log.as_ref().and_then(|p| match std::fs::File::create(p) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("warning: could not open {}: {e}", p.display());
                None
            }
        });
        let sink = file.map(sc_core::JsonLinesSink::new);

        let solver = match &sink {
            Some(s) => AgentSolver::new(&backend).with_sink(s),
            None => AgentSolver::new(&backend),
        };
        let report = Report::new(run_suite(&tasks, &solver));
        println!("{}", report.summary());
        if let Some(p) = &log {
            println!("  log: {}", p.display());
        }
        all_passed &= report.all_passed();
    }

    if all_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
