//! `trace [--check] [--json]` — check the specs against the code (spec 17).
//!
//! The deterministic first layer of drift detection: no model runs, so it costs
//! nothing to run every time and belongs in the check gate next to fmt and
//! clippy. The `spec-guardian` agent stays as the advisory semantic layer above
//! it, reading meaning that anchors cannot capture.

use std::process::ExitCode;

use super::common::workspace;

/// Report the claim table. With `check`, exit non-zero on a broken or stale
/// claim — the CI gate.
///
/// `Unknown` never gates: it means the *checker* could not look, and failing a
/// build over the checker's own limits teaches people to bypass it. `Ungoverned`
/// warns for the same reason spec 17 gives — adding a crate and its spec in one
/// commit is good practice, but a hard failure would block work-in-progress.
pub fn trace(json: bool, check: bool) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    let report = sc_trace::trace(&workspace);
    if json {
        println!("{}", sc_trace::report::json(&report));
    } else {
        print!("{}", sc_trace::report::text(&report));
    }

    if check && report.blocking() > 0 {
        // The summary goes to stderr so it never pollutes a `--json` payload a
        // consumer is parsing.
        eprintln!("\n{}", sc_trace::report::check_summary(&report));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
