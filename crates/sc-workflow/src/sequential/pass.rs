//! The integration passes: the single whole-suite reconcile, the incremental
//! slice-by-slice walk, and the whole-task fallback for a degenerate board.

use std::path::Path;

use sc_core::{
    default_registry, run_agent_observed, select_strategy, AgentConfig, AgentReport, EventSink,
};
use sc_model::ModelBackend;
use sc_proto::Result;
use sc_swarm::{Status, TaskBoard};

use super::report::INTEGRATION_MAX_STEPS;
use super::slice::{cumulative_k, slice_command, FeatureSlice};

/// The final whole-suite integration pass: unfocused, full registry, the real verify
/// command — fix cross-file glue until the frozen suite is green.
pub(super) fn run_integration_pass(
    worker: &dyn ModelBackend,
    task: &str,
    workspace: &Path,
    base_cfg: &AgentConfig,
    sink: &dyn EventSink,
) -> Result<AgentReport> {
    let strategy = select_strategy(&worker.capabilities());
    let registry = default_registry();
    let mut cfg = base_cfg.clone();
    // PIN every source file's full contents (focus = all sources, re-read fresh each turn).
    // This pass reconciles cross-file glue, so it legitimately needs to SEE every file — and
    // the harness already KNOWS the files (they're on disk). Without this it told the model
    // "the source files are written, fix them" but pinned NONE, forcing it to read_file each
    // one repeatedly (observed: app.py read 51× because a read evicts after keep_recent_turns).
    // We name the files AND hand over their contents instead of making it go fetch them.
    cfg.focus_files = sc_core::source_files(workspace);
    cfg.plan_first = false;
    // The convergence loop gets a generous budget — it must verify, read failures, and fix
    // cross-file glue iteratively (but honor a smaller base_cfg.max_steps if the caller set one).
    cfg.max_steps = base_cfg.max_steps.max(INTEGRATION_MAX_STEPS);
    // base_cfg.verify_command is the real pytest oracle — keep it; this pass IS gated.
    let instruction = format!(
        "All the source files for this project are shown below in full (they update after \
         each edit). Make the FULL frozen test suite pass. The tests are FROZEN — do not edit \
         any test file, and do NOT read_file the source files — they are already shown. Run \
         run_verification, read the failures, and fix the SOURCE files — most remaining failures \
         are cross-file glue: a wrong import name between files, a route at the wrong path, or a \
         return-shape mismatch. Keep editing until green, then finish.\n\nProject: {task}"
    );
    run_agent_observed(
        worker,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        workspace,
        &cfg,
        sink,
    )
}

/// Per-slice integration budget: a slice is a SMALL goal (make one feature's tests pass on a
/// green base), so it needs less than the full convergence loop but more than a single write.
const PER_SLICE_MAX_STEPS: usize = 25;

/// Incremental integration: walk the feature slices in dependency order, making each cumulative
/// `-k` slice green before adding the next. Each step pins all source files (the file-handing
/// fix — the model SEES every file; the `-k` only narrows the GOAL) and gates on the SLICED
/// pytest, so the model converges a small feature at a time instead of the whole graph at once.
/// A slice that pre-checks green is skipped (no agent loop); a slice that can't converge in its
/// budget is left for the final full pass (best-effort, like a failed per-file step).
pub(super) fn run_incremental_integration(
    worker: &dyn ModelBackend,
    task: &str,
    workspace: &Path,
    base_cfg: &AgentConfig,
    slices: &[FeatureSlice],
    sink: &dyn EventSink,
) -> Result<Vec<(String, AgentReport)>> {
    let strategy = select_strategy(&worker.capabilities());
    let registry = default_registry();
    let base_verify = base_cfg
        .verify_command
        .as_deref()
        .expect("caller guards verify_command.is_some()");
    let mut steps: Vec<(String, AgentReport)> = Vec::new();

    for i in 0..slices.len() {
        let k = cumulative_k(slices, i);
        let slice_cmd = slice_command(base_verify, &k);

        // Pre-check: if everything built so far already satisfies this slice, advance cheaply
        // (no model turns). Common once an earlier slice's work already covered this feature.
        if sc_verify::run_verification_in(&base_cfg.sandbox, workspace, &slice_cmd).all_green() {
            continue;
        }

        let mut cfg = base_cfg.clone();
        cfg.focus_files = sc_core::source_files(workspace); // see every file in full
        cfg.plan_first = false;
        cfg.max_steps = PER_SLICE_MAX_STEPS;
        cfg.verify_command = Some(slice_cmd); // gate on THIS slice only
        let instruction = format!(
            "All the source files for this project are shown below in full (they update after \
             each edit). Make the tests matching `-k \"{k}\"` pass — this is a GROWING SLICE of \
             the suite (the features built so far). The tests are FROZEN — do not edit any test \
             file, and do NOT read_file the source files (they are already shown). Run \
             run_verification (it is already scoped to this slice), read the failures, and fix \
             the SOURCE files — most failures are cross-file glue: a wrong import name between \
             files, a route at the wrong path, or a return-shape mismatch. Keep editing until \
             this slice is green, then finish.\n\nProject: {task}"
        );
        let report = run_agent_observed(
            worker,
            None,
            &registry,
            strategy.as_ref(),
            &instruction,
            workspace,
            &cfg,
            sink,
        )?;
        steps.push((format!("slice:{k}"), report));
    }
    Ok(steps)
}

/// The whole-task fallback for a degenerate board: today's single-agent behavior over the
/// full task (unfocused, suite-gated). Identical in spirit to the benchmark's `run_pass`.
pub(super) fn run_whole_task(
    worker: &dyn ModelBackend,
    task: &str,
    workspace: &Path,
    base_cfg: &AgentConfig,
    sink: &dyn EventSink,
) -> Result<AgentReport> {
    let strategy = select_strategy(&worker.capabilities());
    let registry = default_registry();
    let mut cfg = base_cfg.clone();
    cfg.focus_files = Vec::new();
    cfg.plan_first = false;
    let instruction = format!(
        "Implement this project so ALL the existing tests pass: {task}\n\n\
         The tests are FROZEN — do not edit any test file. Create every source file the task \
         needs. Use run_verification; keep editing until green, then finish."
    );
    run_agent_observed(
        worker,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        workspace,
        &cfg,
        sink,
    )
}

/// The lowest-id still-`Pending` subtask — the termination guard when `ready()` is empty but
/// work remains (a dependency cycle, or a dep on a failed subtask). Ids are `t1,t2,…` so the
/// min is deterministic.
pub(super) fn lowest_pending(board: &TaskBoard) -> Option<String> {
    board
        .subtasks()
        .iter()
        .filter(|s| s.status == Status::Pending)
        .map(|s| s.id.clone())
        .min()
}

/// Did a step actually produce its target file? With no per-file verify gate, "wrote the
/// file" is the success signal: the run either finished, or its change summary names the
/// scoped file. (`change_summary` comes from the journal of files touched this run.)
pub(super) fn wrote_the_file(report: &AgentReport, files: &[String]) -> bool {
    if report.finished {
        return true;
    }
    files
        .iter()
        .any(|f| report.change_summary.contains(f.as_str()))
}
