//! The single-agent runs: a plain one-shot loop, and the in-place `Iterate` flow
//! that edits real files under a git safety net.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use sc_core::{AgentEvent, FnSink};
use sc_model::ModelBackend;

use crate::bridge::{ChannelConfirmer, Pending};
use crate::config::UiConfig;

use super::UiEvent;

/// Build the backends + config from `cfg` and drive a single-agent run, forwarding
/// every [`AgentEvent`] to the UI — the mirror of `sc-cli::run_task_json` minus the
/// JSON/log sinks, plus the GUI confirmer (Part A).
pub fn run_agent(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(backend) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
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
pub fn run_iterate(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(backend) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
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
            let outcome =
                sc_iterate::finish_summary(&report, &touched, &dirty_at_start, &workspace);
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
