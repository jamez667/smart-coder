//! `dc-core` — the `dumb-coder` agent loop (M0–M1).
//!
//! Drives a `dc_model::ModelBackend` through a bounded act→observe cycle (spec 03),
//! issuing one tool call per turn against a sandboxed workspace (spec 04). It is
//! backend-agnostic and tool-surface-agnostic: the loop is parameterized over a
//! [`dc_tools::ToolRegistry`] and a [`ToolCallStrategy`], so the same loop runs on
//! a `MockBackend` in tests, an Ollama/OpenAI backend on the desktop, or the
//! Android-core `CallbackBackend` on a device — with native function-calling,
//! GBNF-constrained, or plain parse+repair tool decoding (spec 02).
//!
//! `dc-eval` wraps this as a `Solver` so the eval harness can score the real
//! agent on red→green tasks and report its tool-call validity rate.

pub mod advisor;
pub mod agent;
pub mod confirm;
pub mod diagnose;
pub mod runlog;
pub mod event;
pub mod metrics;
pub mod plan;
pub mod planner;
pub mod recovery;
pub mod strategy;

pub use advisor::{advice_observation, consult, Predicament};
pub use diagnose::{diagnose_failure, diagnosis_observation, SourceFile};
pub use runlog::{RunLog, RunLogSink};
pub use agent::{
    run_agent, run_agent_observed, run_agent_recovering, run_agent_with, AgentConfig, AgentReport,
};
pub use confirm::{AutoDeny, Confirmation, Confirmer};
pub use event::{
    AgentEvent, EventSink, FnSink, JsonLinesSink, NullSink, PromptMessage, TeeSink, TranscriptSink,
};
pub use metrics::ToolCallMetrics;
pub use plan::{PlanState, Step, StepStatus};
pub use planner::{make_plan, parse_plan};
pub use recovery::{action_hash, Progress, StallDetector, StopReason};
pub use strategy::{
    extract_json_array, extract_json_object, select_strategy, Grammar, NativeTools, ParseRepair,
    RepairError, ToolCallStrategy,
};

// Re-export the tool surface so downstream crates (dc-eval) get it via dc-core.
pub use dc_tools::{
    default_registry, execute, minimal_worker_registry, Permission, SideEffect, ToolOutcome,
    ToolRegistry, ToolSpec, ValidatedCall, ValidationError,
};
