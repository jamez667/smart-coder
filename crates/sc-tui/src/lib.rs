//! `sc-tui` — a full-screen terminal UI that visualizes a live agent run.
//!
//! It consumes the `sc_core` event stream (spec 01): the agent runs on a worker
//! thread streaming [`sc_core::AgentEvent`]s, and the main thread folds them into
//! a [`TuiState`] and draws the panes — plan, live activity log, metrics, and an
//! honest stop line (spec 06). The state fold is pure and headless-testable; only
//! the render + run harness touch the terminal.

mod app;
mod render;
mod state;

pub use app::{run, TuiRun};
pub use render::draw;
pub use state::{LineKind, LogLine, PlanLine, TuiState};
