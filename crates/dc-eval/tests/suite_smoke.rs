//! End-to-end smoke test over the real on-disk eval suite (`evals/suite.toml`).
//! Proves the harness wiring: load the suite, apply each task's known solution
//! with `FileSolver`, and confirm every task goes red -> green without tampering.

use std::path::PathBuf;

use dc_eval::{run_suite, FileSolver, Report, TaskSuite};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/dc-eval
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

#[test]
fn bundled_suite_passes_with_the_known_solution() {
    let suite_path = repo_root().join("evals").join("suite.toml");
    let suite = TaskSuite::load(&suite_path).expect("load suite");
    assert!(!suite.tasks.is_empty(), "suite should have tasks");

    let report = Report::new(run_suite(&suite.tasks, &FileSolver));
    assert!(
        report.all_passed(),
        "expected all tasks to pass:\n{}",
        report.summary()
    );
}
