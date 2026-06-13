//! `dc-eval` binary — load a task suite, score it with a solver, print a report.
//!
//! Usage:
//!   dc-eval [SUITE_TOML]      (default: evals/suite.toml)
//!
//! For M1 it runs the demo [`FileSolver`] (applies each task's known solution) to
//! prove the harness end-to-end. When the agent loop exists it becomes the
//! solver, driving the Android-core target (or a stand-in) per spec 02.

use std::process::ExitCode;

use dc_eval::{run_suite, FileSolver, Report, TaskSuite};
use dc_model::{AndroidCoreBackend, ModelBackend};

fn main() -> ExitCode {
    let suite_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "evals/suite.toml".to_string());

    // Show the configured primary target up front so the wiring is visible.
    let primary = AndroidCoreBackend::new();
    let avail = if primary.is_available() {
        "available"
    } else {
        "OFF-DEVICE (using demo solver)"
    };
    println!(
        "dumb-coder eval harness\n  primary target: {} [{}]\n  suite: {}\n",
        primary.name(),
        avail,
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
