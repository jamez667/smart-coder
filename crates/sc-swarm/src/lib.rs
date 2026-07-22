//! `sc-swarm` — the worker swarm orchestrator (spec 08).
//!
//! The second core bet of `smart-coder`: instead of one bigger model, run **many
//! tiny workers** on one codebase under a single larger **orchestrator**. The
//! orchestrator decomposes a task into a [`TaskBoard`] of subtasks, runs the
//! independent ones in parallel (each worker is just the `sc_core` agent loop in
//! a scratch copy of the workspace), and **integrates their proposed changes one
//! at a time** with verification after each — parallel intelligence, serialized
//! writes (spec 08).

mod board;
mod decompose;
mod event;
mod orchestrator;
mod worker;

pub use board::{Status, Subtask, TaskBoard};
pub use decompose::{decompose, parse_subtasks, parse_subtasks_on_stack};
pub use event::{FnSwarmSink, NullSwarmSink, SwarmEvent, SwarmSink};
pub use orchestrator::{run_swarm, run_swarm_board, SwarmConfig, SwarmReport};
pub use sc_verify::Sandbox;
pub use worker::{run_worker, run_worker_with_feedback, ProposedChange, WorkerResult};
