//! [`Session`] — spawns an agent or swarm run on a worker thread and streams its
//! events to the UI, exactly the way `dc-cli`/`dc-tui` wire the proven core
//! (`run_agent_observed` / `run_swarm` over a `FnSink`/`FnSwarmSink`, spec 01/06).
//! The GUI is just another front-end: it builds the same backends and config from a
//! [`UiConfig`] and drains a channel of [`UiEvent`]s.
//!
//! Nothing here is an iced type, so the spawn/stream/finish flow is host-testable.
//! The confirm/gate decision seams live in [`crate::bridge`]; this module wires their
//! request channel alongside the event channel.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;

use dc_core::{AgentEvent, FnSink};
use dc_model::ModelBackend;
use dc_swarm::{FnSwarmSink, SwarmEvent};

use crate::bridge::{ChannelConfirmer, Pending};
use crate::config::UiConfig;

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
    /// the test-writing phase (StageBreakdown) lands.
    Phase {
        phase: dc_workflow::Phase,
        content: String,
        tests_written: Vec<String>,
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
}

/// A live run. Holds the receiving ends the UI drains; the worker thread owns the
/// senders and the core. Dropping the `Session` lets the worker finish in the
/// background (its sends become no-ops once the receivers are gone).
pub struct Session {
    events: Receiver<UiEvent>,
    pending: Receiver<Pending>,
    _handle: thread::JoinHandle<()>,
}

impl Session {
    /// Spawn a run on a worker thread. `task` is the user's intent; `workspace` the
    /// repo root. The returned `Session` streams [`UiEvent`]s and [`Pending`] decision
    /// requests until the run ends.
    pub fn spawn(kind: RunKind, cfg: UiConfig, task: String, workspace: PathBuf) -> Self {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let (pending_tx, pending_rx) = crate::bridge::pending_channel();

        let handle = thread::spawn(move || match kind {
            RunKind::Agent => run_agent(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::Swarm => run_swarm(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::Tdd => run_tdd(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::SequentialBuild => {
                run_sequential_build(cfg, task, workspace, ev_tx, pending_tx)
            }
        });

        Self {
            events: ev_rx,
            pending: pending_rx,
            _handle: handle,
        }
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

/// Build the backends + config from `cfg` and drive a single-agent run, forwarding
/// every [`AgentEvent`] to the UI — the mirror of `dc-cli::run_task_json` minus the
/// JSON/log sinks, plus the GUI confirmer (Part A).
fn run_agent(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let backend = cfg.backend();
    let advisor = cfg.advisor();
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));
    let agent_cfg = cfg.agent_config(Some(confirmer));

    let sink = FnSink(|e: &AgentEvent| {
        let _ = ev_tx.send(UiEvent::Agent(e.clone()));
    });

    let result = dc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn dc_model::ModelBackend),
        &registry,
        strategy.as_ref(),
        &task,
        &workspace,
        &agent_cfg,
        &sink,
    );

    match result {
        Ok(report) => {
            let summary = if report.finished {
                format!("finished in {} steps", report.steps)
            } else {
                format!("stopped after {} steps (did not finish)", report.steps)
            };
            let _ = ev_tx.send(UiEvent::Done {
                ok: report.finished,
                summary,
            });
        }
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("run failed: {e}")));
        }
    }
}

/// Build the orchestrator/worker/advisor backends and drive a swarm run, forwarding
/// every [`SwarmEvent`] to the UI — the mirror of `dc-cli::swarm_task_cli`.
fn run_swarm(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let advisor = cfg.swarm_advisor();
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));
    let swarm_cfg = cfg.swarm_config(Some(confirmer));

    let sink = FnSwarmSink(|e: &SwarmEvent| {
        let _ = ev_tx.send(UiEvent::Swarm(e.clone()));
    });

    // Give the decomposer an overview of what's already in the workspace, so when
    // iterating on an existing project it plans edits to existing files (and new
    // files) rather than assuming a blank slate. Empty for a fresh dir.
    let overview = crate::config::repo_overview(&workspace);

    let report = dc_swarm::run_swarm(
        &orchestrator,
        &worker,
        Some(&advisor as &(dyn dc_model::ModelBackend + Sync)),
        &task,
        &overview,
        &workspace,
        &swarm_cfg,
        &sink,
    );

    let summary = format!(
        "{} integrated, {} rejected, {} pending",
        report.done, report.failed, report.pending
    );
    let _ = ev_tx.send(UiEvent::Done {
        ok: report.all_done,
        summary,
    });
}

/// Drive the staged TDD workflow (spec 09/11) then the implementation swarm — the
/// mirror of `dc-cli::plan_task`. The phases stream to the UI as [`UiEvent::Phase`]
/// (the plan panel); after the test-writing phase the swarm implements against the
/// frozen tests until the verify command goes green.
fn run_tdd(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();

    // Each phase artifact lands here as the workflow produces it → the plan panel.
    let phase_tx = ev_tx.clone();
    let on_phase = move |phase: dc_workflow::Phase, content: &str| {
        let _ = phase_tx.send(UiEvent::Phase {
            phase,
            content: content.to_string(),
            tests_written: Vec::new(),
        });
    };

    // Autonomous (AutoApprove) for now — no human gates. Plan → write frozen tests.
    let outcome = match dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        dc_workflow::ThinkPolicy::default(),
        &on_phase,
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("workflow failed: {e}")));
            return;
        }
    };

    // Surface the frozen tests that were written (a real TDD checkpoint to show).
    if !outcome.test_files.is_empty() {
        let _ = ev_tx.send(UiEvent::Phase {
            phase: dc_workflow::Phase::StageBreakdown,
            content: format!("frozen tests written:\n{}", outcome.test_files.join("\n")),
            tests_written: outcome.test_files.clone(),
        });
    }

    if outcome.aborted {
        let _ = ev_tx.send(UiEvent::Done {
            ok: true,
            summary: "plan aborted at a checkpoint — approved artifacts kept".to_string(),
        });
        return;
    }

    // Without a verify command there's nothing to drive the implementation against —
    // stop at the approved plan + frozen tests (a valid TDD halt; spec 09).
    let Some(_) = cfg.verify_command.clone() else {
        let _ = ev_tx.send(UiEvent::Done {
            ok: true,
            summary: format!(
                "plan + {} frozen test file(s) written. Set a verify command to implement.",
                outcome.test_files.len()
            ),
        });
        return;
    };
    if outcome.test_files.is_empty() {
        let _ = ev_tx.send(UiEvent::Failed(
            "no tests were written; nothing to implement against".to_string(),
        ));
        return;
    }

    // IMPLEMENT with a SINGLE agent loop (no swarm, no advisor). One capable model reads
    // the plan + the frozen tests, writes ALL the source files itself, runs the tests,
    // and iterates until green — keeping cross-file coherence the swarm couldn't. The
    // verify command runs every test language (pytest for .py, vitest for *.test.js) in
    // the Docker sandbox so a route test that spans files actually passes.
    let verify_cmd = combined_verify_command(&outcome.test_files);
    let instruction = format!(
        "Implement this project so ALL the existing tests pass: {task}\n\n\
         The tests are already written and FROZEN — do not edit or delete any test file \
         (test_*.py or *.test.js). Read them to learn the exact contract, then write the \
         source files (app.py, templates, static, etc.) to satisfy them. Use \
         run_verification to run the whole suite; keep editing until it is green, then \
         finish.\n\n\
         Plan:\n{}",
        outcome
            .state
            .approved()
            .iter()
            .map(|a| format!("=== {} ===\n{}", a.phase.title(), a.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    );

    let backend = cfg.backend();
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));
    let mut agent_cfg = cfg.agent_config(Some(confirmer));
    agent_cfg.verify_command = Some(verify_cmd);
    // The frozen tests must not be edited by the implementer (spec 11).
    agent_cfg.permission.frozen_paths = outcome.test_files.clone();
    agent_cfg.sandbox = cfg.sandbox();
    // Plan-free: the staged workflow already planned; the agent just implements.
    agent_cfg.plan_first = false;

    let sink = FnSink(|e: &AgentEvent| {
        let _ = ev_tx.send(UiEvent::Agent(e.clone()));
    });
    let report = dc_core::run_agent_observed(
        &backend,
        None, // no advisor — single model
        &registry,
        strategy.as_ref(),
        &instruction,
        &workspace,
        &agent_cfg,
        &sink,
    );

    match report {
        Ok(r) => {
            let _ = ev_tx.send(UiEvent::Done {
                ok: r.finished && r.verified == Some(true),
                summary: if r.verified == Some(true) {
                    format!("all tests green in {} steps", r.steps)
                } else {
                    format!("stopped after {} steps — tests not green", r.steps)
                },
            });
        }
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("implementation failed: {e}")));
        }
    }
}

/// Drive a multi-file build via the SEQUENTIAL per-file driver: plan → write frozen tests →
/// build one file at a time (each step scoped to its file + the contract + a signature map of
/// the others) → a final integration pass. The per-file scoping is what avoids the whole-task
/// file-juggling and the re-read tax. Mirrors `run_tdd`'s plan+test phase, then hands the board
/// to `dc_workflow::build_sequential_with_board` instead of one whole-task agent loop.
fn run_sequential_build(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let phase_tx = ev_tx.clone();
    let on_phase = move |phase: dc_workflow::Phase, content: &str| {
        let _ = phase_tx.send(UiEvent::Phase {
            phase,
            content: content.to_string(),
            tests_written: Vec::new(),
        });
    };

    let outcome = match dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        dc_workflow::ThinkPolicy::default(),
        &on_phase,
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("workflow failed: {e}")));
            return;
        }
    };
    if cfg.verify_command.is_none() {
        let _ = ev_tx.send(UiEvent::Done {
            ok: true,
            summary: "plan + frozen tests written; set a verify command to implement".to_string(),
        });
        return;
    }

    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));
    let mut agent_cfg = cfg.agent_config(Some(confirmer));
    agent_cfg.verify_command = Some(combined_verify_command(&outcome.test_files));
    agent_cfg.permission.frozen_paths = outcome.test_files.clone();
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;

    let sink = FnSink(|e: &AgentEvent| {
        let _ = ev_tx.send(UiEvent::Agent(e.clone()));
    });
    let report = dc_workflow::build_sequential_with_board(
        outcome.board,
        &worker,
        &task,
        &workspace,
        &agent_cfg,
        1, // per-file retry budget
        &sink,
    );
    match report {
        Ok(r) => {
            let _ = ev_tx.send(UiEvent::Done {
                ok: r.verified,
                summary: if r.verified {
                    "all tests green (sequential build)".to_string()
                } else if r.fell_back_whole_task {
                    "built whole-task (degenerate decomposition) — tests not green".to_string()
                } else {
                    format!("built {} file(s) — tests not green", r.per_file.len())
                },
            });
        }
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("sequential build failed: {e}")));
        }
    }
}

/// One verify command that runs every test language present in `test_files`: pytest for
/// `.py` tests, vitest for `*.test.js`. Joined with `&&` so the gate is green only when
/// both pass. (The single agent loop has one verify command; this lets it cover a mixed
/// Python-backend + JS-frontend project.)
fn combined_verify_command(test_files: &[String]) -> String {
    let py: Vec<&String> = test_files.iter().filter(|f| f.ends_with(".py")).collect();
    let js: Vec<&String> = test_files
        .iter()
        .filter(|f| f.ends_with(".test.js"))
        .collect();
    let mut parts = Vec::new();
    if !py.is_empty() {
        // Name the frozen test files explicitly so pytest verifies the CONTRACT, not
        // whatever `test_*.py` happens to sit in the workspace (a stale file from a
        // prior run, or a scratch test the model wrote, must never poison the gate).
        let files = py
            .iter()
            .map(|f| shell_quote(f))
            .collect::<Vec<_>>()
            .join(" ");
        parts.push(format!("python -m pytest -q {files}"));
    }
    if !js.is_empty() {
        let files = js
            .iter()
            .map(|f| shell_quote(f))
            .collect::<Vec<_>>()
            .join(" ");
        parts.push(format!("vitest run {files}"));
    }
    if parts.is_empty() {
        "python -m pytest -q".to_string()
    } else {
        parts.join(" && ")
    }
}

/// Minimal POSIX single-quote (the sandbox runs the command via `sh -c`). Test paths
/// are workspace-relative and tame, but quoting keeps a path with spaces safe.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_core::AgentEvent;

    /// A spawned agent run against an unreachable backend still streams a terminal
    /// `UiEvent` (Failed) rather than hanging — the UI always learns the run ended.
    #[test]
    fn unreachable_backend_yields_a_terminal_event() {
        let cfg = UiConfig {
            // A port nothing listens on ⇒ the backend call errors fast.
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "none".to_string(),
            ..UiConfig::default()
        };
        let ws = std::env::temp_dir();
        let session = Session::spawn(RunKind::Agent, cfg, "do a thing".to_string(), ws);

        // Block for the terminal event by polling the worker to completion.
        let terminal = wait_for_terminal(&session);
        assert!(
            matches!(
                terminal,
                Some(UiEvent::Failed(_)) | Some(UiEvent::Done { .. })
            ),
            "expected a terminal UiEvent, got {terminal:?}"
        );
    }

    /// Drain until a Done/Failed arrives (or the worker thread ends and the channel
    /// closes). Test-only; the real UI drains per-frame.
    fn wait_for_terminal(session: &Session) -> Option<UiEvent> {
        loop {
            match session.events.recv() {
                Ok(ev @ (UiEvent::Done { .. } | UiEvent::Failed(_))) => return Some(ev),
                Ok(_) => continue,     // intermediate event; keep waiting
                Err(_) => return None, // worker ended without a terminal (shouldn't happen)
            }
        }
    }

    #[test]
    fn verify_command_targets_only_the_frozen_tests() {
        // The gate must name the frozen test files, not blanket-collect `test_*.py`
        // — a stale or scratch test in the workspace must never poison verification.
        let cmd =
            combined_verify_command(&["test_app.py".to_string(), "static/app.test.js".to_string()]);
        assert!(
            cmd.contains("pytest -q 'test_app.py'"),
            "pytest scoped to the frozen file: {cmd}"
        );
        assert!(
            cmd.contains("vitest run 'static/app.test.js'"),
            "vitest scoped to the frozen file: {cmd}"
        );
        // No bare whole-directory pytest.
        assert!(
            !cmd.contains("pytest -q &&") && !cmd.trim_end().ends_with("pytest -q"),
            "must not run an unscoped pytest: {cmd}"
        );
    }

    #[test]
    fn py_only_verify_is_scoped() {
        let cmd = combined_verify_command(&["test_app.py".to_string()]);
        assert_eq!(cmd, "python -m pytest -q 'test_app.py'");
    }

    #[test]
    fn ui_event_is_cloneable_for_the_iced_message() {
        // iced Messages must be Clone; UiEvent wraps the (Clone) core events.
        let e = UiEvent::Agent(AgentEvent::ToolCall {
            tool: "read_file".to_string(),
            arg: "src/main.rs".to_string(),
        });
        let _ = e.clone();
    }
}
