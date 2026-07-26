//! The workflow runner (spec 09): drive the pipeline end to end, gating each phase
//! boundary through a [`Gate`], persisting each artifact, and emerging with the
//! swarm-ready subtask board from the final phase.
//!
//! Two gating modes share one loop. The autonomous default ([`run_workflow`])
//! approves every phase via [`AutoApprove`]; an interactive run supplies a gate that
//! reads a human's approve/revise/send-back/abort decision at each checkpoint. The
//! gate is harness-owned — the loop, not the model, applies the decision.
//!
//! Split by concern:
//!
//! * [`mode`] — [`WorkflowMode`] (what to run) and [`WorkflowOutcome`] (what it yields).
//! * [`drive`] — the four public entry points over the one gated loop.
//! * [`ground`] — injecting the plan body and the real files before the first phase.
//! * [`board`] — the decomposition artifact → the swarm's task board.
//!
//! [`Gate`]: crate::gate::Gate
//! [`AutoApprove`]: crate::gate::AutoApprove

mod board;
mod drive;
mod ground;
mod mode;

#[cfg(test)]
mod tests;

pub use drive::{run_workflow, run_workflow_gated, run_workflow_moded, run_workflow_moded_to};
pub use mode::{WorkflowMode, WorkflowOutcome};
