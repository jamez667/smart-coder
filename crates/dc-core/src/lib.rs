//! `dc-core` — the `dumb-coder` agent loop (M0).
//!
//! Drives a `dc_model::ModelBackend` through a bounded act→observe cycle (spec 03),
//! issuing one tool call per turn against a sandboxed workspace (spec 04). It is
//! backend-agnostic: the same loop runs on a `MockBackend` in tests, an Ollama/
//! OpenAI backend on the desktop, or the Android-core `CallbackBackend` on a
//! device.
//!
//! `dc-eval` wraps this as a `Solver` so the eval harness can score the real
//! agent on red→green tasks.

pub mod agent;
pub mod tool;

pub use agent::{run_agent, AgentConfig, AgentReport};
pub use tool::{execute, parse_tool_call, Tool, ToolOutcome};
