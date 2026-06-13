//! `dc-eval` — the M1 eval harness for `dumb-coder`.
//!
//! It answers the make-or-break question of the whole project (spec 00, spec 10):
//! *can a tiny model actually drive a failing test to green, without cheating?*
//!
//! The harness is deliberately decoupled from the agent: it scores any
//! [`solver::Solver`], so it can be exercised today with simple solvers and
//! later wired to the real agent loop (which will drive the Android-core target,
//! or a stand-in, via `dc_model::ModelBackend`).
//!
//! The scoring enforces the TDD invariants from spec 11 — verify-red-first,
//! frozen contract tests, and green-after-solve — so a "pass" is trustworthy.

pub mod fsutil;
pub mod report;
pub mod runner;
pub mod solver;
pub mod task;

pub use report::Report;
pub use runner::{run_suite, run_task, Outcome, TaskResult};
pub use solver::{FileSolver, FnSolver, NoopSolver, Solver};
pub use task::{EvalTask, TaskSuite};
