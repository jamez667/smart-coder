//! The swarm run: decompose the task across parallel workers.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use sc_swarm::{FnSwarmSink, SwarmEvent};

use crate::bridge::{ChannelConfirmer, Pending};
use crate::config::UiConfig;

use super::UiEvent;

/// Build the orchestrator/worker/advisor backends and drive a swarm run, forwarding
/// every [`SwarmEvent`] to the UI — the mirror of `sc-cli::swarm_task_cli`.
pub fn run_swarm(
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
