//! `sc-verify` — run the project's tests and return **structured** results
//! (spec 04 / spec 11).
//!
//! Tests are the control system of `smart-coder`: a small model can't judge its
//! own correctness, but a test can. This crate is the seam that turns a raw test
//! command into the machine-checkable oracle the loop trusts — running the
//! command and parsing per-test pass/fail (cargo, pytest) with a generic
//! exit-code fallback, so `run_verification` always returns something the Context
//! Manager can budget and feed back (failures first).

mod parse;
mod report;
mod run;

pub use parse::{detect, parse, Framework};
pub use report::{TestCase, TestReport};
pub use run::{
    run_command, run_command_in, run_verification, run_verification_in, CommandResult, Sandbox,
};
