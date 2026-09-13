//! The design-pipeline runs: `Plan` (design, stop for review) and `StagedBuild`
//! (design, then compiler-driven build to green).
//!
//! Both stream every phase twice — as a [`UiEvent::Phase`] for the plan panel, and
//! token-by-token into the chat thread so a slow phase reads as alive rather than
//! frozen. That shared streaming setup is [`phase_streams`].

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::bridge::{ChannelGate, Pending};
use crate::config::UiConfig;

use super::verify::sandbox_verify_hint;
use super::UiEvent;

/// The two per-phase callbacks both staged runs need: `on_phase` (a completed
/// artifact → the plan panel) and `on_token` (a generation delta → the chat thread).
///
/// Returned as a pair because they capture their own clones of `ev_tx`; building
/// them together keeps the two runs' streaming behavior identical by construction.
fn phase_streams(
    ev_tx: &Sender<UiEvent>,
    artifact_dir_rel: Option<String>,
) -> (
    impl Fn(sc_workflow::Phase, &str),
    impl FnMut(sc_workflow::Phase, &str),
) {
    let phase_tx = ev_tx.clone();
    let phase_dir = artifact_dir_rel;
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
    let on_token = move |phase: sc_workflow::Phase, delta: &str| {
        // A new phase: finalize nothing here (the last ChatDelta already carries the full reply),
        // just reset the buffer and post the prompt-side header for the new phase.
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

    (on_phase, on_token)
}

/// The "Execute plan" flow: run the staged workflow language-aware and TDD-free through the
/// stage breakdown, streaming each phase to the plan panel, then STOP for review. No frozen
/// tests, no decomposition, no build — the user reads specs → architecture → layout →
/// breakdown and kicks off the build separately. The plan doc the user is executing rides in
/// as the task, so every phase grounds on it.
pub fn run_plan(
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
    // Human-in-the-loop: pause at each design phase for Approve/Send-back via the gatebar/master
    // list — a Breakdown is a REVIEW pass, so it must gate exactly like a staged build (it just
    // stops before the code build). `AutoApprove` would barrel through with nothing to approve.
    let gate = ChannelGate::new(pending_tx);

    // Land the artifacts in the spec's OpenSpec dir when the task references `specs/<slug>/spec.md`,
    // so each phase file (architecture.md, layout.md, …) opens in the code view for review and can
    // carry line-comments for send-back. Falls back to `.smart-coder/plan/` (numbered) otherwise.
    let (artifact_dir, artifact_dir_rel) = sc_workflow::artifact_dirs(&task, &workspace);
    let (on_phase, mut on_token) = phase_streams(&ev_tx, artifact_dir_rel.clone());

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
    let where_ = artifact_dir_rel.unwrap_or_else(|| ".smart-coder/plan/".to_string());
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
pub fn run_staged_build(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
    _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

    // Preflight: warn if the verify command can't run in the chosen sandbox (a Rust project with
    // the default Python image), so the user fixes it BEFORE the build writes files and the verify
    // silently no-ops. Best-effort, non-blocking — the build still proceeds.
    if workspace.join("Cargo.toml").is_file() {
        if let sc_verify::Sandbox::Docker { image } = cfg.sandbox() {
            if image.contains("pyenv") {
                let _ = ev_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
                    role: "system".into(),
                    text: format!(
                        "⚠ This looks like a Rust project, but the sandbox is the Python `{image}` \
                         image — `cargo` won't run there, so the build's verify step will no-op. \
                         Set SC_DOCKER_IMAGE=rust or SC_USE_DOCKER=0 (host) and rebuild."
                    ),
                }));
            }
        }
    }

    // Human-in-the-loop: pause at each design phase (Specs → Architecture → Layout → Breakdown)
    // for Approve/Revise/Send-back via the gatebar, instead of AutoApprove barrelling through.
    let gate = ChannelGate::new(pending_tx);

    // Land the design artifacts NEXT TO the spec in its OpenSpec dir: if the task references
    // `specs/<slug>/spec.md`, phases (architecture.md, layout.md, breakdown.md, …) go in
    // `specs/<slug>/`. Falls back to the default `.smart-coder/plan/` when there's no spec dir.
    let (artifact_dir, artifact_dir_rel) = sc_workflow::artifact_dirs(&task, &workspace);
    let (on_phase, mut on_token) = phase_streams(&ev_tx, artifact_dir_rel);

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

    // 2) Flatten the decomposition into the full build work-list — EVERY subtask, not just the
    // foundational one. (Building only the first dep-free subtask and hoping the compiler surfaces
    // the rest silently stopped after one file when that file compiled cleanly in isolation.)
    let board = outcome.board.subtasks();
    let tasks: Vec<sc_workflow::BuildTask> = board
        .iter()
        .map(|s| sc_workflow::BuildTask {
            id: s.id.clone(),
            goal: s.goal.clone(),
            files: s.files.clone(),
            deps: s.deps.clone(),
        })
        .collect();

    // Surface the decomposition in the chat as a readable plan, so the build's work-list is visible
    // (not just a raw JSON blob buried in the phase stream).
    if tasks.is_empty() {
        let _ = ev_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
            role: "system".into(),
            text: "⚠ decomposition produced no subtasks — building the task as one unit."
                .to_string(),
        }));
    } else {
        let mut summary = format!("🧩 decomposition — {} subtask(s) to build:", tasks.len());
        for t in &tasks {
            summary.push_str(&format!("\n  • {} — {}", t.id, t.goal));
        }
        let _ = ev_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
            role: "system".into(),
            text: summary,
        }));
    }

    // Pick the verify command the project ACTUALLY needs: `iterate_verify_command` detects the
    // stack (Cargo.toml → `cargo check`, package.json → npm, Python → pytest) and overrides a stale
    // pytest default. It MUST be called first — using `cfg.verify_command` directly meant a Rust
    // project inherited the default `python -m pytest -q`, so the compiler-driven fix loop ran
    // pytest (which finds no rust errors), saw "0 errors", declared green, and NEVER fixed the
    // real compile errors the edits introduced (observed live 2026-07-21: a build left the tree
    // uncompilable because the fix loop was checking with the wrong tool).
    let verify = sc_iterate::iterate_verify_command(&cfg.verify_command, &workspace)
        .or_else(|| cfg.verify_command.clone())
        .unwrap_or_else(|| "cargo check".to_string());

    // 3) Compiler-driven build: apply EVERY subtask in dependency order, then cargo-check→fix each
    // diagnostic until green. Tee progress into the chat as system notes.
    let build_tx = ev_tx.clone();
    let build_task = task.clone();
    let on_build = move |ev: sc_workflow::BuildEvent| {
        let note = match ev {
            sc_workflow::BuildEvent::Foundational { goal } => format!("▶ building: {goal}"),
            sc_workflow::BuildEvent::Subtask {
                id,
                goal,
                index,
                total,
            } => format!("▶ [{index}/{total}] {id}: {goal}"),
            sc_workflow::BuildEvent::Checked { errors } => {
                format!("● cargo check → {errors} error(s)")
            }
            sc_workflow::BuildEvent::Fixing {
                file,
                line,
                message,
            } => format!("  ↳ fix {file}:{line} — {message}"),
            sc_workflow::BuildEvent::Done { green, iterations } => {
                format!(
                    "build {} after {iterations} iteration(s)",
                    if green { "GREEN ✓" } else { "incomplete" }
                )
            }
        };
        let _ = build_tx.send(UiEvent::Agent(sc_core::AgentEvent::ChatMessage {
            role: "system".into(),
            text: note,
        }));
    };

    // Forward the scoped-edit agent's events into the UI stream, so the desktop counts touched
    // files (the "N files built" banner) and shows each edit in the chat / code view live —
    // previously these were swallowed, so a successful build reported "0 files touched".
    let agent_tx = ev_tx.clone();
    let on_agent = move |e: &sc_core::AgentEvent| {
        let _ = agent_tx.send(UiEvent::Agent(e.clone()));
    };

    // A single fallback task when the board is empty, so an un-decomposed change still builds.
    let fallback = [sc_workflow::BuildTask {
        id: "t1".to_string(),
        goal: build_task,
        files: Vec::new(),
        deps: Vec::new(),
    }];
    let build_tasks = if tasks.is_empty() {
        &fallback[..]
    } else {
        &tasks[..]
    };

    let result = sc_workflow::build_all_subtasks(
        &worker,
        &workspace,
        &cfg.sandbox(),
        &verify,
        build_tasks,
        &on_build,
        &on_agent,
    );

    // Report the outcome. Distinguish "the verify command couldn't RUN" (non-zero exit but zero
    // parseable compile errors — typically the wrong sandbox, e.g. `cargo` missing in the Python
    // image) from a genuine compile failure. Otherwise a build that actually wrote its files reads
    // as "incomplete, 0 errors", which is baffling.
    let summary = if result.green {
        format!(
            "built ✓ — verify green in {} iteration(s)",
            result.iterations
        )
    } else if result.remaining.is_empty() {
        // Not green, yet nothing parseable failed ⇒ the verify command itself didn't run.
        let hint = sandbox_verify_hint(&cfg, &verify, &workspace);
        format!("files were written, but the verify step couldn't run — {hint}")
    } else {
        format!(
            "stopped with {} compile error(s) after {} iteration(s)",
            result.remaining.len(),
            result.iterations
        )
    };
    let _ = ev_tx.send(UiEvent::Done {
        // Treat "verify couldn't run" as ok=true for the banner: the files were written; the
        // failure is environmental, not the build's fault. The message says what to fix.
        ok: result.green || result.remaining.is_empty(),
        summary,
    });
}
