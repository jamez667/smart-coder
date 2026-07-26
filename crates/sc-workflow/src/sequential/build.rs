//! The build driver: plan (or accept a board), then walk it ONE file at a time.
//!
//! The whole-task loop fails on multi-file builds for a harness reason, not a model one —
//! a capable model emits the entire solution as one batched turn and the loop keeps only
//! the first call. Scoping each step to a single file removes that mismatch entirely.

use std::path::Path;

use sc_core::{run_agent_observed, select_strategy, AgentConfig, AgentReport, EventSink};
use sc_model::ModelBackend;
use sc_proto::Result;
use sc_swarm::TaskBoard;

use crate::policy::ThinkPolicy;
use crate::runner::run_workflow;

use super::pass::{
    lowest_pending, run_incremental_integration, run_integration_pass, run_whole_task,
    wrote_the_file,
};
use super::report::{
    per_file_registry, read_frozen_contract, SequentialReport, PER_FILE_MAX_STEPS,
};
use super::slice::{derive_slices, parse_test_names};

/// Full entry point: run the staged workflow to get the decomposition, then drive it
/// sequentially. For callers (GUI/CLI) that want the whole pipeline. The benchmark uses
/// [`build_sequential_with_board`] instead, so it can inject a frozen oracle between
/// planning and the per-file walk.
#[allow(clippy::too_many_arguments)]
pub fn build_sequential(
    orchestrator: &dyn ModelBackend,
    worker: &dyn ModelBackend,
    task: &str,
    workspace: &Path,
    base_cfg: &AgentConfig,
    think: ThinkPolicy,
    per_file_retry_budget: usize,
    sink: &dyn EventSink,
) -> Result<SequentialReport> {
    let outcome = run_workflow(orchestrator, worker, task, workspace, think, &|_, _| {})?;
    build_sequential_with_board(
        outcome.board,
        worker,
        task,
        workspace,
        base_cfg,
        per_file_retry_budget,
        sink,
    )
}

/// Drive a PRE-COMPUTED decomposition board sequentially. Separated from
/// [`build_sequential`] so a caller can run the workflow, swap in a frozen oracle, and only
/// then drive the per-file walk (the A/B benchmark does exactly this).
pub fn build_sequential_with_board(
    mut board: TaskBoard,
    worker: &dyn ModelBackend,
    task: &str,
    workspace: &Path,
    base_cfg: &AgentConfig,
    per_file_retry_budget: usize,
    sink: &dyn EventSink,
) -> Result<SequentialReport> {
    let board_rendered = board.render();

    // Degenerate board ⇒ decomposition gave us nothing to split across files (empty, or the
    // documented single whole-task fallback). Run today's whole-task behavior so we never
    // regress a simple single-file task into needless ceremony.
    let degenerate = board.is_empty() || (board.len() == 1 && board.subtasks()[0].files.is_empty());
    if degenerate {
        let final_pass = run_whole_task(worker, task, workspace, base_cfg, sink)?;
        let verified = final_pass.verified == Some(true);
        return Ok(SequentialReport {
            board_rendered,
            fell_back_whole_task: true,
            per_file: Vec::new(),
            incremental: Vec::new(),
            final_pass,
            verified,
        });
    }

    let strategy = select_strategy(&worker.capabilities());
    // A verify-less write/edit/finish registry: the per-file step writes ONE file then
    // finishes; without run_verification it can't dead-end on the (intentionally absent)
    // verify command. It still has write_file (a per-file step must CREATE the file).
    let registry = per_file_registry();
    // Read the frozen contract ONCE and show it to every per-file step, so it writes real
    // logic matching the asserted shapes — not stubs. Safe: frozen_paths still denies edits
    // to these tests, and the per-file registry has no run_verification/run_command, so a step
    // can neither run nor weaken them.
    let contract = read_frozen_contract(workspace, &base_cfg.permission.frozen_paths);
    let mut per_file: Vec<(String, AgentReport)> = Vec::new();

    // Walk the board in dependency order. Each iteration ends in complete/fail, strictly
    // reducing the pending count, so the loop terminates in ≤ board.len() steps even with a
    // dependency cycle (the lowest-pending guard breaks a stuck `ready()`).
    loop {
        let next_id = match board.ready().into_iter().next() {
            Some(id) => id,
            None => match lowest_pending(&board) {
                Some(id) => id, // cycle / dead dep: run it anyway rather than strand it
                None => break,  // nothing pending → done
            },
        };
        let st = board
            .subtasks()
            .iter()
            .find(|s| s.id == next_id)
            .expect("ready id is a real subtask")
            .clone();
        board.claim(&st.id);

        let cfg = per_file_cfg(base_cfg, PER_FILE_MAX_STEPS, &st.files);
        let instruction = per_file_instruction(&st.files, &st.goal, &contract);

        // Retry budget: a weak first attempt gets one more scoped try before we give up on
        // the file and let the final pass try to rescue it.
        let mut attempt = 0;
        let report = loop {
            let r = run_agent_observed(
                worker,
                None,
                &registry,
                strategy.as_ref(),
                &instruction,
                workspace,
                &cfg,
                sink,
            )?;
            attempt += 1;
            if wrote_the_file(&r, &st.files) || attempt > per_file_retry_budget {
                break r;
            }
        };

        if wrote_the_file(&report, &st.files) {
            board.complete(&st.id);
        } else {
            board.fail(&st.id);
        }
        per_file.push((st.id, report));
    }

    // Integration. Instead of asking the model to make the WHOLE suite green at once (which
    // oscillates on many-file apps — fixing one cross-file bug reveals another, observed flat
    // at 9-failed for 80 cycles on the 8-file S3), build the app up one FEATURE at a time and
    // keep it green — standard engineering practice. The frozen tests slice by feature in
    // dependency order (`routes_authors.py` → `-k author`, …), so each step closes a SMALL new
    // slice on an already-green base. When the app has no `routes_<feature>.py` files (or no
    // tests for them) — S1/S2, non-Flask — `derive_slices` is empty and we fall back to today's
    // single full pass, so the passing rungs don't regress.
    let test_names = parse_test_names(&contract);
    let slices = derive_slices(&board, &test_names);
    let incremental = if slices.is_empty() || base_cfg.verify_command.is_none() {
        Vec::new()
    } else {
        run_incremental_integration(worker, task, workspace, base_cfg, &slices, sink)?
    };

    // The final full-suite pass always runs: it's the last feature's closer (catalog/glue) AND
    // the backstop if an incremental slice didn't fully converge. After the slices, the earlier
    // features are green, so it only has the residue to fix.
    let final_pass = run_integration_pass(worker, task, workspace, base_cfg, sink)?;
    let verified = final_pass.verified == Some(true);

    Ok(SequentialReport {
        board_rendered,
        fell_back_whole_task: false,
        per_file,
        incremental,
        final_pass,
        verified,
    })
}

/// The config for a per-file step: clone the base, drop the suite gate, cap the steps, and
/// FOCUS on the subtask's file(s). Focusing pins the file's live contents every turn (and the
/// files it imports), and the harness short-circuits a `read_file` of an already-pinned file —
/// killing the re-read tax (the per-file step otherwise re-reads its own file + imports
/// reflexively). The FOCUS_TASK_PREFIX handles greenfield: when the file doesn't exist yet
/// nothing is pinned, and the instruction tells the model to `write_file` it.
pub(super) fn per_file_cfg(base: &AgentConfig, max_steps: usize, files: &[String]) -> AgentConfig {
    let mut cfg = base.clone();
    cfg.plan_first = false;
    cfg.focus_files = files.to_vec();
    // Per-file steps are NOT gated on the frozen suite: the suite imports `from app import
    // app`, so until EVERY file exists it errors at collection for reasons unrelated to the
    // file being written. Gating an early step on it is incoherent (can never be green yet).
    // The suite is the single source of truth, checked once — in the final pass.
    cfg.verify_command = None;
    cfg.max_steps = max_steps;
    cfg
}

/// The per-file instruction: write exactly one file to satisfy BOTH its decomposition goal
/// AND the frozen test contract. Showing the contract is the whole point — without it the
/// model only has a vague goal and writes stubs; with it, it writes real logic matching the
/// exact shapes/status codes/routes the tests assert. The "other files may not exist yet"
/// caveat stays (the suite can't pass until they all exist); the old "tests not your concern /
/// no tests to run" framing is GONE — it told the model to ignore the one thing that defines
/// what its file must do.
pub(super) fn per_file_instruction(files: &[String], goal: &str, contract: &str) -> String {
    let file = files.join(", ");
    let contract_block = if contract.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\nThe project's tests are FROZEN (you cannot edit or run them here — they are \
             shown ONLY as the CONTRACT your code must satisfy). `{file}` must make the parts \
             of these tests that exercise it pass — match the EXACT return shapes, status \
             codes, route paths, and function signatures they assert:\n\n```python\n{contract}\n\
             ```\n"
        )
    };
    format!(
        "Write ONLY the file `{file}` and nothing else this run. Implement it FULLY and \
         correctly — real working logic, never a stub or `pass` — to satisfy this goal:\n\
         {goal}{contract_block}\n\
         The OTHER source files may not exist yet, so you cannot run the whole suite now; just \
         implement `{file}` completely to its goal AND the contract above. Do not create or \
         edit any other file. If `{file}` does not exist, create it with `write_file` (the \
         ENTIRE contents in one shot); if it exists, edit it. When `{file}` is written \
         correctly to the contract, call `finish`."
    )
}
