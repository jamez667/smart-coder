//! [`Session`] — spawns an agent or swarm run on a worker thread and streams its
//! events to the UI, exactly the way `sc-cli`/`sc-tui` wire the proven core
//! (`run_agent_observed` / `run_swarm` over a `FnSink`/`FnSwarmSink`, spec 01/06).
//! The GUI is just another front-end: it builds the same backends and config from a
//! [`UiConfig`] and drains a channel of [`UiEvent`]s.
//!
//! Nothing here is an iced type, so the spawn/stream/finish flow is host-testable.
//! The confirm/gate decision seams live in [`crate::bridge`]; this module wires their
//! request channel alongside the event channel.
//!
//! One module per run family:
//!
//! * [`agent`] — the single-agent run and the in-place `Iterate` flow.
//! * [`staged`] — the design pipeline: plan-only, and plan→compiler-driven build.
//! * [`tdd`] — the frozen-test flows: TDD and the sequential per-file build.
//! * [`swarm`] — the decompose-and-parallelize run.
//! * [`slug`] — task text → the `specs/<slug>/` artifact directory.
//! * [`verify`] — verify-command assembly and its sandbox diagnostics.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::thread;

use crate::bridge::Pending;
use crate::config::UiConfig;

mod agent;
mod slug;
mod staged;
mod swarm;
mod tdd;
mod verify;

#[cfg(test)]
mod tests;

use sc_core::AgentEvent;
use sc_swarm::SwarmEvent;

/// Everything the UI receives from a run: the live event streams, the terminal
/// summary, and a fatal error. Pending confirm/gate *requests* travel on their own
/// [`Pending`] channel (see [`Session::pending`]) so the UI can answer them.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A single-agent run event (spec 03/06 vocabulary).
    Agent(AgentEvent),
    /// A swarm orchestrator event (spec 08 vocabulary).
    Swarm(SwarmEvent),
    /// A staged-workflow phase completed (spec 09): the phase and its full artifact
    /// text. Drives the plan panel. `tests_written` lists the frozen test files once
    /// the test-writing phase (StageBreakdown) lands. `dir` is the WORKSPACE-RELATIVE
    /// directory the artifacts land in (e.g. `specs/alt-seats`) so the plan's master list
    /// can open each phase's file in the code view and harvest line-comments on it for
    /// send-back; `None` for run kinds with no OpenSpec dir (older `.smart-coder/plan/`).
    Phase {
        phase: sc_workflow::Phase,
        content: String,
        tests_written: Vec<String>,
        dir: Option<String>,
    },
    /// The run finished. `ok` is the honest exit status (finished/all-done); `summary`
    /// is the human closing line (spec 06).
    Done { ok: bool, summary: String },
    /// The run could not start or panicked (backend unreachable, etc.).
    Failed(String),
}

/// What kind of run to launch.
pub enum RunKind {
    /// A single-agent run over one instruction.
    Agent,
    /// A swarm run that decomposes the task across workers.
    Swarm,
    /// The staged TDD workflow (spec 09/11): plan → write frozen tests → swarm
    /// implements against them until green.
    Tdd,
    /// Multi-file build via the sequential per-file driver: plan → write frozen tests →
    /// build ONE file at a time (each step scoped to its file + the contract + a signature
    /// map of the others) → a final integration pass. Avoids the whole-task file-juggling
    /// (and the re-read tax) by scoping each step to a single file.
    SequentialBuild,
    /// Iterate on an EXISTING project in place (the daily-driver flow): no spec/test
    /// ceremony — the single agent reads the relevant files, edits them, runs the configured
    /// verify command (e.g. `cargo check`) until it's green, then finishes. This is the mode
    /// the GUI uses when you've picked a project folder to work in.
    Iterate,
    /// Plan-only: run the staged workflow through the stage breakdown (language-aware, no
    /// frozen tests) and STOP for review — the "Execute plan" flow. Produces specs →
    /// architecture → layout → breakdown as reviewable artifacts; does not build.
    Plan,
    /// The full "plan → build" flow: run the staged pipeline through decomposition (no tests),
    /// then hand its foundational chunk to the compiler-driven executor, which applies it and
    /// loops cargo-check→fix-each-diagnostic until green. The daily-driver for a real change.
    StagedBuild,
}

/// A live run. Holds the receiving ends the UI drains; the worker thread owns the
/// senders and the core. Dropping the `Session` lets the worker finish in the
/// background (its sends become no-ops once the receivers are gone).
pub struct Session {
    events: Receiver<UiEvent>,
    pending: Receiver<Pending>,
    /// Cooperative cancel flag shared with the run: `cancel()` flips it and the agent loop
    /// stops at its next turn boundary.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _handle: thread::JoinHandle<()>,
}

impl Session {
    /// Spawn a run on a worker thread. `task` is the user's intent; `workspace` the
    /// repo root. The returned `Session` streams [`UiEvent`]s and [`Pending`] decision
    /// requests until the run ends.
    pub fn spawn(kind: RunKind, cfg: UiConfig, task: String, workspace: PathBuf) -> Self {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let (pending_tx, pending_rx) = crate::bridge::pending_channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_worker = cancel.clone();

        let handle = thread::spawn(move || match kind {
            RunKind::Agent => agent::run_agent(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::Swarm => swarm::run_swarm(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::Tdd => tdd::run_tdd(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::SequentialBuild => {
                tdd::run_sequential_build(cfg, task, workspace, ev_tx, pending_tx)
            }
            RunKind::Iterate => {
                agent::run_iterate(cfg, task, workspace, ev_tx, pending_tx, cancel_worker)
            }
            RunKind::Plan => staged::run_plan(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::StagedBuild => {
                staged::run_staged_build(cfg, task, workspace, ev_tx, pending_tx, cancel_worker)
            }
        });

        Self {
            events: ev_rx,
            pending: pending_rx,
            cancel,
            _handle: handle,
        }
    }

    /// Request cancellation: the agent loop stops at its next turn boundary. Idempotent.
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Non-blocking drain of any events that have arrived since the last call.
    pub fn drain_events(&self) -> Vec<UiEvent> {
        self.events.try_iter().collect()
    }

    /// Non-blocking drain of any pending decision requests (confirm/gate).
    pub fn drain_pending(&self) -> Vec<Pending> {
        self.pending.try_iter().collect()
    }
}
