//! `sc-verify` — run the project's tests and return **structured** results
//! (spec 04 / spec 11).
//!
//! Tests are the control system of `smart-coder`: a small model can't judge its
//! own correctness, but a test can. This crate is the seam that turns a raw test
//! command into the machine-checkable oracle the loop trusts — running the
//! command and parsing per-test pass/fail (cargo, pytest) with a generic
//! exit-code fallback, so `run_verification` always returns something the Context
//! Manager can budget and feed back (failures first).

mod delta;
mod parse;
mod report;
mod run;

/// The verification delta ("same N failures as last run"), for a caller that runs the
/// suite itself and parses the output but still wants the same comparison
/// `run_verification_in` makes.
pub use delta::{forget_runs, note_run};
pub use parse::{detect, parse, Framework};
pub use report::{compile_checklist, compile_errors, CompileError, TestCase, TestReport};
pub use run::{
    host_shell, run_command, run_command_full, run_command_in, run_verification,
    run_verification_in, shell_path, strip_leading_cd, CommandResult, Sandbox, SessionContainer,
    OUTPUT_BYTE_CAP,
};
