//! The frozen-test runs: the TDD flow (one agent implements against written tests)
//! and the sequential per-file build.
//!
//! Both share the plan→write-frozen-tests opening; they differ in what implements
//! afterwards — one whole-task agent loop, or the per-file driver.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use sc_core::{AgentEvent, FnSink};
use sc_model::ModelBackend;

use crate::bridge::{ChannelConfirmer, Pending};
use crate::config::UiConfig;

use super::verify::combined_verify_command;
use super::UiEvent;

/// Drive the staged TDD workflow (spec 09/11) then the implementation — the
/// mirror of `sc-cli::plan_task`. The phases stream to the UI as [`UiEvent::Phase`]
/// (the plan panel); after the test-writing phase a single agent implements against
/// the frozen tests until the verify command goes green.
pub fn run_tdd(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let Some(orchestrator) = cfg.orchestrator() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(worker) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };

    // Each phase artifact lands here as the workflow produces it → the plan panel.
    let phase_tx = ev_tx.clone();
    let on_phase = move |phase: sc_workflow::Phase, content: &str| {
        let _ = phase_tx.send(UiEvent::Phase {
            phase,
            content: content.to_string(),
            tests_written: Vec::new(),
            dir: None, // TDD flow uses the default plan dir — no OpenSpec file to open
        });
    };

    // Autonomous (AutoApprove) for now — no human gates. Plan → write frozen tests.
    let outcome = match sc_workflow::run_workflow(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        sc_workflow::ThinkPolicy::default(),
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
            phase: sc_workflow::Phase::StageBreakdown,
            content: format!("frozen tests written:\n{}", outcome.test_files.join("\n")),
            tests_written: outcome.test_files.clone(),
            dir: None,
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

    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(backend) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
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
    let report = sc_core::run_agent_observed(
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
/// file-juggling and the re-read tax. Mirrors [`run_tdd`]'s plan+test phase, then hands the board
/// to `sc_workflow::build_sequential_with_board` instead of one whole-task agent loop.
pub fn run_sequential_build(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let Some(orchestrator) = cfg.orchestrator() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(worker) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    let phase_tx = ev_tx.clone();
    let on_phase = move |phase: sc_workflow::Phase, content: &str| {
        let _ = phase_tx.send(UiEvent::Phase {
            phase,
            content: content.to_string(),
            tests_written: Vec::new(),
            dir: None, // sequential build uses the default plan dir — no OpenSpec file to open
        });
    };

    let outcome = match sc_workflow::run_workflow(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        sc_workflow::ThinkPolicy::default(),
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
    let report = sc_workflow::build_sequential_with_board(
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
