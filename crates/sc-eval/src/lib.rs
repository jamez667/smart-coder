//! `sc-eval` — the M1 eval harness for `smart-coder`.
//!
//! It answers the make-or-break question of the whole project (spec 00, spec 10):
//! *can a tiny model actually drive a failing test to green, without cheating?*
//!
//! The harness is deliberately decoupled from the agent: it scores any
//! [`solver::Solver`], so it can be exercised today with simple solvers and
//! later wired to the real agent loop (which drives the configured backend via
//! `sc_model::ModelBackend`).
//!
//! The scoring enforces the TDD invariants from spec 11 — verify-red-first,
//! frozen contract tests, and green-after-solve — so a "pass" is trustworthy.

pub mod fsutil;
pub mod gateway_arm;
pub mod pi_arm;
pub mod raw_arm;
pub mod report;
pub mod results;
pub mod retrieval;
pub mod runner;
pub mod solver;
pub mod task;

pub use gateway_arm::{build_arm, Arm, ArmRun, ArmSolver, AskStats, GatewayTool};
pub use report::{ab_report, Report};
pub use results::{
    current_commit, rung_of, runtime_head, stale_binary_warning, warn_if_stale, write_rows,
    MetricsSink, ResultRow, RunMetrics,
};
pub use retrieval::{QueryResult, RetrievalQuery, RetrievalSuite};
pub use runner::{run_suite, run_task, Outcome, TaskResult};
pub use solver::{AgentSolver, FileSolver, FnSolver, NoopSolver, Solver};
pub use task::{EvalTask, TaskSuite};
