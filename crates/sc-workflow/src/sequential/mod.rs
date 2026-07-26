//! Sequential per-file build (spec 03/08 — decomposition WITHOUT the parallel swarm).
//!
//! The whole-task agent loop fails on multi-file builds for a harness reason, not a model
//! one: a capable coder model emits the ENTIRE solution as one batched turn (20-40 tool
//! calls — create every file + verify), and the loop runs exactly ONE call per turn and
//! discards the rest (`ParseRepair::extract`). The model re-emits its files each turn and
//! the harness drops them again — a long grind to land what it wrote correctly in turn 1.
//!
//! The fix is to never hand the model the whole task. We reuse the decomposition the staged
//! workflow already produces (`WorkflowOutcome.board` — one `Subtask` per file, with deps)
//! and drive it with a SINGLE agent, ONE file at a time, in dependency order. No parallel
//! workers, no advisor, no worktrees, no integration-merge — just the agent loop scoped to
//! one file per step, then a final whole-suite pass to reconcile cross-file glue.
//!
//! This is the "decomposition kept, multi-agent shelved" shape: the decomposition was always
//! the valuable part; the parallel execution is what was dropped.
//!
//! Split by concern:
//!
//! * [`report`] — [`SequentialReport`], the step budgets, and the per-file tool registry.
//! * [`build`] — the two entry points and the per-file walk.
//! * [`pass`] — the integration passes (single, incremental, whole-task fallback).
//! * [`slice`] — deriving the cumulative feature slices the incremental pass walks.

mod build;
mod pass;
mod report;
mod slice;

#[cfg(test)]
mod tests;

pub use build::{build_sequential, build_sequential_with_board};
pub use report::SequentialReport;
