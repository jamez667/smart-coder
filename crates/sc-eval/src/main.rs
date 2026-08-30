//! `sc-eval` binary — score a solver on a task suite, print a report.
//!
//! Two suites:
//!
//!     sc-eval [SUITE_TOML]              the red->green demo suite (FileSolver)
//!     sc-eval --agent [SUITE_TOML] [--url <U>] [--model <M>]   the same suite, real model
//!     sc-eval --swebench [--live] --url <U> --model <M> [--only <ID>] [--json <PATH>]
//!
//! The SWE-bench path runs the real agent loop against a live backend, one instance
//! per container. See [`sc_eval::swebench`] for what is and is not measured.

use std::process::ExitCode;

use sc_eval::swebench::{run_instance, InstanceRun, Subset, SweAgentSolver};
use sc_eval::{run_suite, AgentSolver, FileSolver, Report, TaskSuite};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--swebench") {
        return swebench(&args);
    }
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

fn swebench(args: &[String]) -> ExitCode {
    let url = flag(args, "--url").unwrap_or("http://localhost:11436/v1");
    let model = flag(args, "--model").unwrap_or("tiel-coder-35b");
    let only = flag(args, "--only");
    let json_out = flag(args, "--json");

    // Which benchmark. They are different measurements and never mix.
    let live = args.iter().any(|a| a == "--live");
    let subset = match if live {
        Subset::bundled_live()
    } else {
        Subset::bundled()
    } {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let instances: Vec<_> = match only {
        Some(id) => match subset.get(id) {
            Some(i) => vec![i.clone()],
            None => {
                eprintln!("error: {id} is not in the vendored subset");
                return ExitCode::FAILURE;
            }
        },
        None => subset.instances.clone(),
    };

    // `with_detected_context` adopts the server's real n_ctx; without it the backend
    // assumes 8192 and the context budgeter would work to the wrong number.
    let backend = sc_model::OpenAiBackend::new(url, model)
        .with_detected_context()
        .with_native_tools();
    let solver = SweAgentSolver::new(&backend).with_verbose(args.iter().any(|a| a == "--verbose"));

    println!(
        "smart-coder SWE-bench\n  model: {model} @ {url}\n  subset: {} of {} ({})\n",
        instances.len(),
        subset.total_in_split,
        subset.source
    );

    let started = std::time::Instant::now();
    let mut runs: Vec<InstanceRun> = Vec::new();
    for (n, inst) in instances.iter().enumerate() {
        print!("[{}/{}] {} ... ", n + 1, instances.len(), inst.instance_id);
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let run = run_instance(inst, &solver);
        match (&run.harness_error, run.resolved) {
            (Some(e), _) => println!("HARNESS ERROR — {e}"),
            (None, true) => println!("RESOLVED    {}", run.score.line()),
            (None, false) => println!("unresolved  {}", run.score.line()),
        }
        runs.push(run);
    }

    // Harness errors are reported separately and NOT averaged in as misses: an image
    // that would not start says nothing about the model.
    let errored = runs.iter().filter(|r| r.harness_error.is_some()).count();
    let scored = runs.len() - errored;
    let resolved = runs.iter().filter(|r| r.resolved).count();

    println!(
        "\n{resolved}/{scored} resolved{}  ({:.0}s)",
        if errored > 0 {
            format!("  [{errored} harness errors, excluded]")
        } else {
            String::new()
        },
        started.elapsed().as_secs_f64()
    );

    if let Some(path) = json_out {
        let doc = serde_json::json!({
            "model": model,
            "endpoint": url,
            "subset_source": subset.source,
            "subset_size": instances.len(),
            "total_in_split": subset.total_in_split,
            "note": subset.note,
            "resolved": resolved,
            "scored": scored,
            "harness_errors": errored,
            "wall_clock_secs": started.elapsed().as_secs(),
            "instances": runs,
        });
        match std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
            Ok(()) => println!("wrote {path}"),
            Err(e) => {
                eprintln!("error writing {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // A harness error is a failure of the run, not a score of zero.
    if errored > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
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

    let backend = sc_model::OpenAiBackend::new(url, model);
    let solver = AgentSolver::new(&backend);
    let report = Report::new(run_suite(&suite.tasks, &solver));
    println!("{}", report.summary());

    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
