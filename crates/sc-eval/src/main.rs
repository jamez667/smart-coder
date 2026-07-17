//! `sc-eval` binary — load a task suite, score it with a solver, print a report.
//!
//! Usage:
//!   sc-eval [SUITE_TOML]      (default: evals/suite.toml)
//!
//! For M1 it runs the demo [`FileSolver`] (applies each task's known solution) to
//! prove the harness end-to-end. When the agent loop exists it becomes the
//! solver, driving the configured backend per spec 02.

use std::process::ExitCode;

use sc_eval::{run_suite, FileSolver, Report, TaskSuite};

fn main() -> ExitCode {
    let suite_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "evals/suite.toml".to_string());

    println!(
        "smart-coder eval harness\n  solver: demo FileSolver\n  suite: {}\n",
        suite_path
    );

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
