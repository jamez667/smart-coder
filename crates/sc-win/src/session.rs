//! [`Session`] — spawns an agent or swarm run on a worker thread and streams its
//! events to the UI, exactly the way `sc-cli`/`sc-tui` wire the proven core
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

use sc_core::{AgentEvent, FnSink};
use sc_model::ModelBackend;
use sc_swarm::{FnSwarmSink, SwarmEvent};

use crate::bridge::{ChannelConfirmer, ChannelGate, Pending};
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
            RunKind::Agent => run_agent(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::Swarm => run_swarm(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::Tdd => run_tdd(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::SequentialBuild => {
                run_sequential_build(cfg, task, workspace, ev_tx, pending_tx)
            }
            RunKind::Iterate => run_iterate(cfg, task, workspace, ev_tx, pending_tx, cancel_worker),
            RunKind::Plan => run_plan(cfg, task, workspace, ev_tx, pending_tx),
            RunKind::StagedBuild => {
                run_staged_build(cfg, task, workspace, ev_tx, pending_tx, cancel_worker)
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

/// Build the backends + config from `cfg` and drive a single-agent run, forwarding
/// every [`AgentEvent`] to the UI — the mirror of `sc-cli::run_task_json` minus the
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
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));
    let agent_cfg = cfg.agent_config(Some(confirmer));

    let sink = FnSink(|e: &AgentEvent| {
        let _ = ev_tx.send(UiEvent::Agent(e.clone()));
    });

    let result = sc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn sc_model::ModelBackend),
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

/// Drive an ITERATE run **safely, via git**: the agent edits the real files (fast — it reuses
/// your `target/` cache for an incremental `cargo check`), but the harness tracks exactly
/// which files it touches, and if the run ends **not green**, those files are `git checkout`-
/// reverted. So a broken/truncated intermediate is never *left* on disk — either you get a
/// verified change, or your files are restored to their committed state. (This replaces a
/// full scratch copy, which would be painfully slow on a large repo.)
///
/// Verify runs on the HOST (`cargo check` needs the real toolchain); nothing is frozen.
fn run_iterate(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let backend = cfg.backend();
    let advisor = cfg.advisor();
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));

    let mut agent_cfg = cfg.agent_config(Some(confirmer));
    // The iterate flavor (verify-command detection, no-ceremony overrides) is shared with the
    // remote server via `sc-iterate`, so both front-ends behave identically.
    sc_iterate::apply_iterate_overrides(&mut agent_cfg, &cfg.verify_command, &workspace);
    // Wire the Cancel button's flag so the loop can stop between turns.
    agent_cfg.cancel = Some(cancel);

    let instruction = sc_iterate::iterate_instruction(&task, &workspace);

    // Files that already have uncommitted changes BEFORE this run. We must never auto-revert
    // one of these (that would wipe the user's own work) — only files that were clean.
    let dirty_at_start = sc_iterate::git_dirty_files(&workspace);

    // Track the files the agent edits (from the event stream), so on failure we revert
    // exactly those — not the whole tree — leaving unrelated uncommitted work alone.
    let edited: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        Default::default();
    let edited_sink = edited.clone();
    let ev_tx_sink = ev_tx.clone();
    let sink = FnSink(move |e: &AgentEvent| {
        if let AgentEvent::ToolCall { tool, arg } = e {
            if matches!(
                tool.as_str(),
                "write_file" | "create_file" | "edit_file" | "append_file"
            ) {
                let p = arg.trim();
                if !p.is_empty() {
                    edited_sink.lock().unwrap().insert(p.replace('\\', "/"));
                }
            }
        }
        let _ = ev_tx_sink.send(UiEvent::Agent(e.clone()));
    });
    let result = sc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn sc_model::ModelBackend),
        &registry,
        strategy.as_ref(),
        &instruction,
        &workspace,
        &agent_cfg,
        &sink,
    );

    let touched: Vec<String> = edited.lock().unwrap().iter().cloned().collect();

    // Accept-or-revert decision + closing line — shared with the remote server via `sc-iterate`.
    match result {
        Ok(report) => {
            let outcome = sc_iterate::finish_summary(&report, &touched, &dirty_at_start, &workspace);
            let _ = ev_tx.send(UiEvent::Done {
                ok: outcome.ok,
                summary: outcome.summary,
            });
        }
        Err(e) => {
            // A hard error mid-run: revert the files that were CLEAN before the run (never
            // ones the user had uncommitted work in).
            let safe: Vec<String> = touched
                .iter()
                .filter(|f| !dirty_at_start.contains(*f))
                .cloned()
                .collect();
            sc_iterate::git_revert_files(&workspace, &safe);
            let _ = ev_tx.send(UiEvent::Failed(format!(
                "iterate failed: {e} (reverted {} clean file(s))",
                safe.len()
            )));
        }
    }
}


/// Build the orchestrator/worker/advisor backends and drive a swarm run, forwarding
/// every [`SwarmEvent`] to the UI — the mirror of `sc-cli::swarm_task_cli`.
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

    let report = sc_swarm::run_swarm(
        &orchestrator,
        &worker,
        Some(&advisor as &(dyn sc_model::ModelBackend + Sync)),
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
/// mirror of `sc-cli::plan_task`. The phases stream to the UI as [`UiEvent::Phase`]
/// (the plan panel); after the test-writing phase the swarm implements against the
/// frozen tests until the verify command goes green.
/// The "Execute plan" flow: run the staged workflow language-aware and TDD-free through the
/// stage breakdown, streaming each phase to the plan panel, then STOP for review. No frozen
/// tests, no decomposition, no build — the user reads specs → architecture → layout →
/// breakdown and kicks off the build separately. The plan doc the user is executing rides in
/// as the task, so every phase grounds on it.
fn run_plan(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    // Human-in-the-loop: pause at each design phase for Approve/Send-back via the gatebar/master
    // list — a Breakdown is a REVIEW pass, so it must gate exactly like a staged build (it just
    // stops before the code build). `AutoApprove` would barrel through with nothing to approve.
    let gate = ChannelGate::new(pending_tx);

    // Land the artifacts in the spec's OpenSpec dir when the task references `specs/<slug>/spec.md`,
    // so each phase file (architecture.md, layout.md, …) opens in the code view for review and can
    // carry line-comments for send-back. Falls back to `.smart-coder/plan/` (numbered) otherwise.
    let artifact_dir = spec_artifact_dir(&task, &workspace);
    let artifact_dir_rel = artifact_dir.as_ref().and_then(|d| {
        d.strip_prefix(&workspace)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    });

    let phase_tx = ev_tx.clone();
    let phase_dir = artifact_dir_rel.clone();
    let on_phase = move |phase: sc_workflow::Phase, content: &str| {
        let _ = phase_tx.send(UiEvent::Phase {
            phase,
            content: content.to_string(),
            tests_written: Vec::new(),
            dir: phase_dir.clone(),
        });
    };

    // Stream each phase's generation LIVE into the chat thread (same as `run_staged_build`), so a
    // Breakdown run reads as alive token-by-token instead of frozen. A "you"-side header per phase,
    // then the reply grows as ChatDelta. (This is why `run_plan` uses `run_workflow_moded_to` with
    // an explicit token callback rather than the no-op `run_workflow_moded` delegator.)
    let chat_tx = ev_tx.clone();
    let mut cumulative = String::new();
    let mut last_phase: Option<sc_workflow::Phase> = None;
    let mut on_token = move |phase: sc_workflow::Phase, delta: &str| {
        if last_phase != Some(phase) {
            cumulative.clear();
            last_phase = Some(phase);
            let _ = chat_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
                role: "you".into(),
                text: format!("▶ {} — generating…", phase.title()),
            }));
        }
        cumulative.push_str(delta);
        let _ = chat_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatDelta {
            cumulative: cumulative.clone(),
        }));
    };

    let outcome = match sc_workflow::run_workflow_moded_to(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        sc_workflow::ThinkPolicy::default(),
        sc_workflow::WorkflowMode::plan_only(),
        &on_phase,
        &gate,
        artifact_dir.as_deref(),
        artifact_dir.is_some(), // OpenSpec filenames when writing into specs/<slug>/
        &mut on_token,
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("planning failed: {e}")));
            return;
        }
    };

    // Aborted at a gate → stop; keep the approved design.
    if outcome.aborted {
        let _ = ev_tx.send(UiEvent::Done {
            ok: true,
            summary: "stopped at a checkpoint — approved design kept".to_string(),
        });
        return;
    }

    let phases = outcome.state.approved().len();
    let where_ = artifact_dir_rel
        .clone()
        .unwrap_or_else(|| ".smart-coder/plan/".to_string());
    let _ = ev_tx.send(UiEvent::Done {
        ok: true,
        summary: format!(
            "plan ready — {phases} design phase(s) in {where_}. Review the breakdown, then build."
        ),
    });
}

/// The full plan→build flow: run the staged pipeline through decomposition, then drive the
/// compiler-driven executor to green. This is the disciplined path — design first, then build in
/// tiny compiler-verified steps — replacing the bare iterate loop for a real change.
/// If `task` references a `specs/<slug>/spec.md`, return the absolute `<workspace>/specs/<slug>/`
/// directory so the design phases land beside the spec (OpenSpec layout). `None` otherwise (the
/// workflow then uses its default `.smart-coder/plan/`).
fn spec_artifact_dir(task: &str, workspace: &std::path::Path) -> Option<PathBuf> {
    let token = task
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ','))
        .map(|t| t.trim_end_matches('.').replace('\\', "/"))
        .find(|t| t.to_ascii_lowercase().starts_with("specs/") && t.to_ascii_lowercase().ends_with("/spec.md"))?;
    // Strip the trailing `/spec.md` to get the feature directory.
    let dir_rel = token.strip_suffix("/spec.md").or_else(|| token.strip_suffix("/SPEC.MD"))?;
    Some(workspace.join(dir_rel))
}

fn run_staged_build(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
    _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    // Human-in-the-loop: pause at each design phase (Specs → Architecture → Layout → Breakdown)
    // for Approve/Revise/Send-back via the gatebar, instead of AutoApprove barrelling through.
    let gate = ChannelGate::new(pending_tx);

    // Land the design artifacts NEXT TO the spec in its OpenSpec dir: if the task references
    // `specs/<slug>/spec.md`, phases (architecture.md, layout.md, breakdown.md, …) go in
    // `specs/<slug>/`. Falls back to the default `.smart-coder/plan/` when there's no spec dir.
    let artifact_dir = spec_artifact_dir(&task, &workspace);
    // The WORKSPACE-RELATIVE form of the artifact dir (e.g. `specs/alt-seats`), for the UI:
    // the plan's master list opens each phase file (`<dir>/<openspec_filename>`) in the code
    // view and harvests line-comments on it for send-back — both need the path `select_file`
    // uses, which is workspace-relative with forward slashes. `None` when there's no spec dir.
    let artifact_dir_rel = artifact_dir.as_ref().and_then(|d| {
        d.strip_prefix(&workspace)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    });

    // Stream each design phase into the plan panel as it lands.
    let phase_tx = ev_tx.clone();
    let phase_dir = artifact_dir_rel.clone();
    let on_phase = move |phase: sc_workflow::Phase, content: &str| {
        let _ = phase_tx.send(UiEvent::Phase {
            phase,
            content: content.to_string(),
            tests_written: Vec::new(),
            dir: phase_dir.clone(),
        });
    };

    // Stream the model's per-phase generation LIVE into the chat thread, so a staged run reads as
    // alive (token by token) instead of sitting frozen while a slow phase generates. For each
    // phase we emit a "you"-side header the moment its first token arrives, then grow the reply as
    // ChatDelta (the FULL cumulative text so far — the app renders the last delta as the live
    // bubble). The design artifacts still stream to the PLAN panel via `on_phase`; this is the
    // separate chat back-and-forth the user asked to see.
    let chat_tx = ev_tx.clone();
    let mut cumulative = String::new();
    let mut last_phase: Option<sc_workflow::Phase> = None;
    let mut on_token = move |phase: sc_workflow::Phase, delta: &str| {
        // A new phase: finalize nothing here (the last ChatDelta already carries the full reply —
        // see below), just reset the buffer and post the prompt-side header for the new phase.
        if last_phase != Some(phase) {
            cumulative.clear();
            last_phase = Some(phase);
            let _ = chat_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
                role: "you".into(),
                text: format!("▶ {} — generating…", phase.title()),
            }));
        }
        cumulative.push_str(delta);
        // Emit the growing reply. The app folds ChatDelta into its live "typing" bubble; the final
        // delta of a phase leaves the full reply on screen, so no terminal ChatMessage is needed
        // (a terminal message would duplicate the text). The next phase's header ends this turn.
        let _ = chat_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatDelta {
            cumulative: cumulative.clone(),
        }));
    };

    // 1) Design pipeline through decomposition (no frozen tests).
    let mode = sc_workflow::WorkflowMode {
        skip_tests: true,
        stop_after: None,
    };
    let outcome = match sc_workflow::run_workflow_moded_to(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        sc_workflow::ThinkPolicy::default(),
        mode,
        &on_phase,
        &gate,
        artifact_dir.as_deref(),
        artifact_dir.is_some(), // OpenSpec filenames when writing into specs/<slug>/
        &mut on_token,
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("planning failed: {e}")));
            return;
        }
    };

    // If the user aborted at a gate, stop here — keep the approved design, don't build.
    if outcome.aborted {
        let _ = ev_tx.send(UiEvent::Done {
            ok: true,
            summary: "stopped at a checkpoint — approved design kept, not built".to_string(),
        });
        return;
    }

    // 2) The foundational chunk: the first dep-free subtask (the one change the decomposition can
    // name; the compiler discovers the rest). Fall back to the whole task if the board is empty.
    let board = outcome.board.subtasks();
    let (found_goal, found_files) = board
        .iter()
        .find(|s| s.deps.is_empty())
        .or_else(|| board.first())
        .map(|s| (s.goal.clone(), s.files.clone()))
        .unwrap_or_else(|| (task.clone(), Vec::new()));

    let verify = cfg
        .verify_command
        .clone()
        .or_else(|| sc_iterate::iterate_verify_command(&cfg.verify_command, &workspace))
        .unwrap_or_else(|| "cargo check".to_string());

    // 3) Compiler-driven build: apply the foundational chunk, then cargo-check→fix each diagnostic
    // until green. Tee progress into the activity stream as chat notes.
    let build_tx = ev_tx.clone();
    let on_build = move |ev: sc_workflow::BuildEvent| {
        let note = match ev {
            sc_workflow::BuildEvent::Foundational { goal } => format!("▶ building: {goal}"),
            sc_workflow::BuildEvent::Checked { errors } => {
                format!("● cargo check → {errors} error(s)")
            }
            sc_workflow::BuildEvent::Fixing {
                file,
                line,
                message,
            } => format!("  ↳ fix {file}:{line} — {message}"),
            sc_workflow::BuildEvent::Done { green, iterations } => {
                format!("build {} after {iterations} iteration(s)", if green { "GREEN ✓" } else { "incomplete" })
            }
        };
        let _ = build_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
            role: "system".into(),
            text: note,
        }));
    };

    let result = sc_workflow::build_compiler_driven(
        &worker,
        &workspace,
        &cfg.sandbox(),
        &verify,
        &found_goal,
        &found_files,
        &on_build,
    );

    let _ = ev_tx.send(UiEvent::Done {
        ok: result.green,
        summary: if result.green {
            format!("built ✓ — cargo check green in {} iteration(s)", result.iterations)
        } else {
            format!(
                "stopped with {} compile error(s) after {} iteration(s)",
                result.remaining.len(),
                result.iterations
            )
        },
    });
}

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

    let backend = cfg.backend();
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
/// file-juggling and the re-read tax. Mirrors `run_tdd`'s plan+test phase, then hands the board
/// to `sc_workflow::build_sequential_with_board` instead of one whole-task agent loop.
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
    use sc_core::AgentEvent;

    #[test]
    fn spec_artifact_dir_resolves_the_openspec_folder() {
        let ws = std::path::Path::new("/proj");
        // A task naming specs/<slug>/spec.md → the artifact dir is specs/<slug>/.
        let d = spec_artifact_dir("Design how to implement specs/alt-seats/spec.md.", ws);
        assert_eq!(d, Some(ws.join("specs").join("alt-seats")));
        // No spec reference → None (falls back to the default plan dir).
        assert_eq!(spec_artifact_dir("just build something", ws), None);
    }

    /// Run a git command in `dir`, ignoring failures (test setup).
    fn git(dir: &std::path::Path, args: &[&str]) {
        let _ = crate::proc::git()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output();
    }

    #[test]
    fn git_revert_restores_a_clean_file_and_dirty_detection_protects_uncommitted() {
        // Build a tiny real git repo to exercise the safety helpers end to end.
        let dir = std::env::temp_dir().join(format!("dc-git-safe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "committed-a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "committed-b\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        // Tree is clean now.
        assert!(sc_iterate::git_dirty_files(&dir).is_empty(), "clean after commit");

        // User has uncommitted work in b.txt; a.txt is clean.
        std::fs::write(dir.join("b.txt"), "MY UNCOMMITTED WORK\n").unwrap();
        let dirty = sc_iterate::git_dirty_files(&dir);
        assert!(dirty.contains("b.txt"), "b.txt seen dirty: {dirty:?}");
        assert!(!dirty.contains("a.txt"), "a.txt still clean");

        // Simulate the agent breaking BOTH files.
        std::fs::write(dir.join("a.txt"), "BROKEN-a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "BROKEN-b\n").unwrap();

        // On failure we revert ONLY the file that was clean (a.txt), never b.txt.
        let touched = ["a.txt".to_string(), "b.txt".to_string()];
        let safe: Vec<String> = touched
            .iter()
            .filter(|f| !dirty.contains(*f))
            .cloned()
            .collect();
        assert_eq!(
            safe,
            vec!["a.txt".to_string()],
            "only the clean file is safe"
        );
        assert!(sc_iterate::git_revert_files(&dir, &safe));

        // a.txt restored to committed; b.txt's uncommitted work is UNTOUCHED (not reverted).
        // (Compare trimmed — git may normalize line endings on Windows checkout.)
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap().trim(),
            "committed-a",
            "clean file reverted to committed"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap().trim(),
            "BROKEN-b",
            "dirty file left as-is (its uncommitted work not destroyed by a blind revert)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_helpers_are_safe_outside_a_repo() {
        let dir = std::env::temp_dir().join(format!("dc-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Not a git repo → dirty set empty, revert reports false (caller warns).
        assert!(sc_iterate::git_dirty_files(&dir).is_empty());
        assert!(!sc_iterate::git_revert_files(&dir, &["x.txt".to_string()]));
        // Empty list is a no-op success.
        assert!(sc_iterate::git_revert_files(&dir, &[]));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
