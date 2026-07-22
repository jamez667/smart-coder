//! `sc-workflow` — the staged workflow & checkpoints (spec 09).
//!
//! `smart-coder` doesn't jump from a one-line request to editing code. For a
//! non-trivial task it runs a fixed pipeline — specs → architecture → layout →
//! stage breakdown → work decomposition — producing a compact approved artifact at
//! each phase that grounds the next. The stage breakdown carries the ordered build
//! sequence AND the concrete per-stage steps (it subsumes the old separate
//! implementation-plan phase). The final phase emits the subtask board the swarm
//! consumes ([`sc_swarm`]).
//!
//! Phases are produced by the single-agent reasoning loop ([`sc_core`]) on the
//! orchestrator (T1) model. Gates are **harness-owned**: the model can't
//! self-approve or skip a phase. A [`Gate`] decides each checkpoint — [`AutoApprove`]
//! for a headless run, or a channel-backed gate that pauses for human
//! Approve/Send-back/Abort (the desktop GUI's staged Breakdown/Build flow).

mod compile_driven;
mod coverage;
mod engine;
mod gate;
mod phase;
mod policy;
mod runner;
mod sequential;
mod stack;
mod staged;
mod state;
mod testwriter;

pub use compile_driven::{
    build_all_subtasks, build_compiler_driven, BuildEvent, BuildOutcome, BuildTask,
};
pub use coverage::{group_by_file, parse_coverage, CoverageItem};
pub use engine::{generate_phase, phase_messages};
pub use gate::{AutoApprove, CeremonyGate, Decision, Gate};
pub use phase::{Ceremony, Phase, PhaseSet};
pub use policy::ThinkPolicy;
pub use runner::{
    run_workflow, run_workflow_gated, run_workflow_moded, run_workflow_moded_to, WorkflowMode,
    WorkflowOutcome,
};
pub use stack::ProjectStack;
pub use staged::{parse_stages, staged_build, Stage, StageResult, StagedReport};
pub use sequential::{build_sequential, build_sequential_with_board, SequentialReport};
pub use state::{load, load_from, plan_dir, save, Artifact, Status, WorkflowState};
pub use testwriter::{persist_tests, write_tests, WrittenTest};
