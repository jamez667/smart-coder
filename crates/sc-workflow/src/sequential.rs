//! Sequential per-file build (spec 03/08 — decomposition WITHOUT the parallel swarm).
//!
//! The whole-task agent loop fails on multi-file builds for a harness reason, not a model
//! one: a capable coder model emits the ENTIRE solution as one batched turn (20-40 tool
//! calls — create every file + verify), and the loop runs exactly ONE call per turn and
//! discards the rest (`ParseRepair::extract`). The model re-emits its files each turn and
//! the harness drops them again — a long grind to land what it wrote correctly in turn 1.
//!
//! The fix is to never hand the model the whole task. We reuse the decomposition the staged
//! workflow already produces (`WorkflowOutcome.board` — one `Subtask` per file, with deps)
//! and drive it with a SINGLE agent, ONE file at a time, in dependency order. No parallel
//! workers, no advisor, no worktrees, no integration-merge — just the agent loop scoped to
//! one file per step, then a final whole-suite pass to reconcile cross-file glue.
//!
//! This is the "decomposition kept, multi-agent shelved" shape: the decomposition was always
//! the valuable part; the parallel execution is what was dropped.

use std::path::Path;

use sc_core::{
    default_registry, run_agent_observed, select_strategy, AgentConfig, AgentReport, EventSink,
    ToolRegistry,
};
use sc_model::ModelBackend;
use sc_proto::Result;
use sc_swarm::{Status, TaskBoard};

use crate::policy::ThinkPolicy;
use crate::runner::run_workflow;

/// What a sequential build did, for reporting/inspection.
pub struct SequentialReport {
    /// The decomposition board, rendered (for logs).
    pub board_rendered: String,
    /// True when the board was degenerate (empty / single file-less subtask) and we fell
    /// back to the whole-task behavior instead of a per-file walk.
    pub fell_back_whole_task: bool,
    /// Per-file step outcomes, in execution order: (subtask id, its agent report).
    pub per_file: Vec<(String, AgentReport)>,
    /// Incremental integration steps in order: (label e.g. "slice:author or book", report).
    /// Empty when slicing wasn't applicable (single-file app / no keyworded tests → the single
    /// full pass below was used instead).
    pub incremental: Vec<(String, AgentReport)>,
    /// The final whole-suite integration pass (the one place cross-file glue is fixed).
    pub final_pass: AgentReport,
    /// Whether the final whole-suite verification was green.
    pub verified: bool,
}

/// Per-file step budget — TINY: the step's only job is to write one file, which a capable
/// model does in turn 1. With a verify-less registry (no run_verification to dead-end on),
/// it then calls `finish`. A small cap keeps a confused step from burning budget if it
/// doesn't finish promptly — the file is already written, so we move on.
const PER_FILE_MAX_STEPS: usize = 5;

/// The integration pass gets the lion's share of the budget: it's the convergence loop that
/// must run the suite, read failures, and fix cross-file glue until green.
const INTEGRATION_MAX_STEPS: usize = 60;

/// The per-file registry: write/edit/finish, but deliberately NO `run_verification` or
/// `run_command`. Per-file steps run with `verify_command = None`, so a `run_verification`
/// call would return "no verification configured" and the model would dead-end on it instead
/// of finishing (observed live: every per-file step wrote its file in turn 1, then stalled
/// ~15 turns calling run_verification/run_command). Removing the tool removes the trap —
/// after writing the file the only sensible move left is `finish`.
fn per_file_registry() -> ToolRegistry {
    let specs = default_registry()
        .specs()
        .iter()
        .filter(|s| s.name != "run_verification" && s.name != "run_command")
        .cloned()
        .collect();
    ToolRegistry::new(specs)
}

/// Read the frozen test contract the per-file steps must satisfy — the asserts that pin what
/// each file must return / which status codes / which routes. WITHOUT this a per-file step
/// only gets a vague decomposition goal ("implement save and resolve") and writes STUBS
/// (`def save(url): pass`), which the integration pass can't rescue (observed live via a
/// prompt dump). Prefer the explicit `frozen_paths` (the human/workflow-approved contract
/// tests); fall back to a shallow `test_*.py` glob of the workspace root. "" if none found.
fn read_frozen_contract(workspace: &Path, frozen_paths: &[String]) -> String {
    let mut parts: Vec<String> = frozen_paths
        .iter()
        .filter_map(|rel| std::fs::read_to_string(workspace.join(rel)).ok())
        .collect();
    if parts.is_empty() {
        if let Ok(entries) = std::fs::read_dir(workspace) {
            let mut hits: Vec<(String, String)> = Vec::new();
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let Some(n) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if n.starts_with("test_") && n.ends_with(".py") {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        hits.push((n.to_string(), s));
                    }
                }
            }
            hits.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic order
            parts = hits.into_iter().map(|(_, s)| s).collect();
        }
    }
    parts.join("\n\n")
}

/// A feature slice: a pytest `-k` keyword derived from a route file's name, plus that file.
/// The incremental integration walks these in dependency order, making each cumulative slice
/// (`-k "author or book"`) green before adding the next — so the model only ever closes a SMALL
/// new slice on a green base, never the whole multi-file graph at once.
#[derive(Debug, Clone, PartialEq)]
struct FeatureSlice {
    keyword: String,
    #[allow(dead_code)] // kept for reporting/debugging; the keyword is what drives -k
    file: String,
}

/// Map a source file to its pytest `-k` keyword, by FILENAME convention (not prose). A
/// `routes_<feature>.py` blueprint → `<feature>` (singularized): `routes_authors.py` → "author".
/// Infrastructure (`store`/`service`/`app`) and glue (templates/static) → `None`: they aren't a
/// testable feature on their own — they're folded into the first feature's base or caught by the
/// final full-suite pass. Returns `None` for anything not matching `routes_*.py`, which is what
/// makes single-`routes.py` apps (S1/S2) fall back to today's single integration pass.
fn feature_keyword(file: &str) -> Option<String> {
    let name = std::path::Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    let stem = name.strip_suffix(".py")?;
    let feature = stem.strip_prefix("routes_")?;
    if feature.is_empty() {
        return None;
    }
    // Singularize a trailing plural so the keyword matches test names (`test_create_author`,
    // not `authors`). Only the simple `s` case — the test oracles use singular feature nouns.
    let singular = feature.strip_suffix('s').unwrap_or(feature);
    if singular.is_empty() {
        return None;
    }
    Some(singular.to_string())
}

/// Extract `def test_<name>` names from the frozen contract string (already loaded — no I/O).
fn parse_test_names(contract: &str) -> Vec<String> {
    contract
        .lines()
        .filter_map(|line| {
            let t = line.trim_start();
            let rest = t.strip_prefix("def ")?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.starts_with("test_") {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

/// Subtask ids in dependency order — a topological walk over `deps`, INDEPENDENT of current
/// status (the build loop has already marked everything Complete by the time slices are derived,
/// so we can't reuse `ready()`, which keys off Pending). Emits a subtask once all its deps have
/// been emitted; falls back to the lowest remaining id to break a cycle/dangling dep. Order
/// matches the build walk's because both follow deps.
fn ordered_subtask_ids(board: &TaskBoard) -> Vec<String> {
    let ids: Vec<String> = board.subtasks().iter().map(|s| s.id.clone()).collect();
    let mut emitted: Vec<String> = Vec::new();
    while emitted.len() < ids.len() {
        let next = board
            .subtasks()
            .iter()
            .filter(|s| !emitted.contains(&s.id))
            .find(|s| s.deps.iter().all(|d| emitted.contains(d)))
            .map(|s| s.id.clone())
            .or_else(|| {
                // cycle / dep on a missing id: take the lowest remaining id, don't strand it.
                ids.iter()
                    .filter(|id| !emitted.contains(*id))
                    .min()
                    .cloned()
            });
        match next {
            Some(id) => emitted.push(id),
            None => break,
        }
    }
    emitted
}

/// Derive the ordered feature slices for incremental integration: each `routes_<feature>.py` in
/// dependency order whose keyword actually appears in a frozen test name. A keyword with no
/// matching test is skipped (nothing to verify); duplicates are de-duped preserving dep order.
/// Empty ⇒ the app has no `routes_<feature>.py` files (or no tests for them) ⇒ caller falls back
/// to today's single full integration pass.
fn derive_slices(board: &TaskBoard, test_names: &[String]) -> Vec<FeatureSlice> {
    let has_test = |kw: &str| {
        let kw = kw.to_lowercase();
        test_names.iter().any(|t| t.to_lowercase().contains(&kw))
    };
    let mut slices: Vec<FeatureSlice> = Vec::new();
    for id in ordered_subtask_ids(board) {
        let Some(st) = board.subtasks().iter().find(|s| s.id == id) else {
            continue;
        };
        for file in &st.files {
            if let Some(keyword) = feature_keyword(file) {
                if has_test(&keyword) && !slices.iter().any(|s| s.keyword == keyword) {
                    slices.push(FeatureSlice {
                        keyword,
                        file: file.clone(),
                    });
                }
            }
        }
    }
    slices
}

/// The cumulative `-k` expression for slices `0..=upto`: `"author"`, `"author or book"`, …
fn cumulative_k(slices: &[FeatureSlice], upto: usize) -> String {
    slices[..=upto]
        .iter()
        .map(|s| s.keyword.as_str())
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Append a pytest `-k "<expr>"` filter to the base verify command, so a slice runs only the
/// tests for the features built so far: `python -m pytest -q 'test_app.py'` → `… -k "author"`.
fn slice_command(base: &str, k: &str) -> String {
    format!("{base} -k \"{k}\"")
}

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
fn per_file_cfg(base: &AgentConfig, max_steps: usize, files: &[String]) -> AgentConfig {
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
fn per_file_instruction(files: &[String], goal: &str, contract: &str) -> String {
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

/// The final whole-suite integration pass: unfocused, full registry, the real verify
/// command — fix cross-file glue until the frozen suite is green.
fn run_integration_pass(
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
fn run_incremental_integration(
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
fn run_whole_task(
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
fn lowest_pending(board: &TaskBoard) -> Option<String> {
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
fn wrote_the_file(report: &AgentReport, files: &[String]) -> bool {
    if report.finished {
        return true;
    }
    files
        .iter()
        .any(|f| report.change_summary.contains(f.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_core::FnSink;
    use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
    use sc_proto::Result as DcResult;
    use sc_swarm::Subtask;
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// A backend that records every instruction it was asked to act on, and replays a fixed
    /// reply (default: write the file named in the instruction, then finish — so per-file
    /// steps "succeed" deterministically without a real model).
    struct SpyBackend {
        seen_instructions: Mutex<Vec<String>>,
        // Each call: emit a write_file for the FIRST `path` the instruction names + finish.
        script: RefCell<Vec<String>>,
    }
    impl SpyBackend {
        fn new() -> Self {
            Self {
                seen_instructions: Mutex::new(Vec::new()),
                script: RefCell::new(Vec::new()),
            }
        }
    }
    impl ModelBackend for SpyBackend {
        fn name(&self) -> &str {
            "spy"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, req: &GenerateRequest) -> DcResult<GenerateResponse> {
            // The user message carries the instruction; record it once per turn.
            let instr = req
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            self.seen_instructions.lock().unwrap().push(instr.clone());
            // If the instruction names a file to write, write it then finish. Else finish.
            // Parse the backtick-quoted `path` from "Write ONLY the file `x`".
            let path = instr
                .split("Write ONLY the file `")
                .nth(1)
                .and_then(|s| s.split('`').next())
                .map(|s| s.to_string());
            let content = match path {
                Some(p) if !self.script.borrow().contains(&p) => {
                    self.script.borrow_mut().push(p.clone());
                    format!("{{\"tool\":\"write_file\",\"path\":\"{p}\",\"content\":\"# {p}\\n\"}}")
                }
                _ => "{\"tool\":\"finish\"}".to_string(),
            };
            Ok(GenerateResponse { content })
        }
    }

    fn ws(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-wf-seq-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn base_cfg() -> AgentConfig {
        AgentConfig {
            // No verify command at the base → the final pass is ungated too (verified=None),
            // which is fine for these structural tests (no Docker, we assert ordering/scoping).
            verify_command: None,
            ..AgentConfig::default()
        }
    }

    #[test]
    fn walks_the_board_in_dependency_order_one_file_each() {
        let dir = ws("order");
        let board = TaskBoard::new(vec![
            Subtask::new("t3", "build c")
                .with_files(vec!["c.py".into()])
                .with_deps(vec!["t1".into(), "t2".into()]),
            Subtask::new("t1", "build a").with_files(vec!["a.py".into()]),
            Subtask::new("t2", "build b")
                .with_files(vec!["b.py".into()])
                .with_deps(vec!["t1".into()]),
        ]);
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        let rep =
            build_sequential_with_board(board, &spy, "task", &dir, &base_cfg(), 1, &sink).unwrap();

        assert!(!rep.fell_back_whole_task);
        // Per-file steps ran in dep order a → b → c.
        let order: Vec<&str> = rep.per_file.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["t1", "t2", "t3"], "dep order");
        // Each per-file instruction named exactly its own file.
        let seen = spy.seen_instructions.lock().unwrap();
        assert!(seen.iter().any(|i| i.contains("`a.py`")));
        assert!(seen.iter().any(|i| i.contains("`b.py`")));
        assert!(seen.iter().any(|i| i.contains("`c.py`")));
        // The files were actually written to disk by the per-file steps.
        assert!(
            dir.join("a.py").exists() && dir.join("b.py").exists() && dir.join("c.py").exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn degenerate_board_falls_back_to_whole_task() {
        let dir = ws("degen");
        // Single subtask with NO files = the documented decomposition fallback.
        let board = TaskBoard::new(vec![Subtask::new("t1", "do the whole thing")]);
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        let rep =
            build_sequential_with_board(board, &spy, "whole task", &dir, &base_cfg(), 1, &sink)
                .unwrap();
        assert!(
            rep.fell_back_whole_task,
            "degenerate board → whole-task fallback"
        );
        assert!(rep.per_file.is_empty(), "no per-file steps in fallback");
        // The whole-task instruction (not a per-file one) was used.
        let seen = spy.seen_instructions.lock().unwrap();
        assert!(seen.iter().any(|i| i.contains("Implement this project")));
        assert!(!seen.iter().any(|i| i.contains("Write ONLY the file")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_board_falls_back_and_terminates() {
        let dir = ws("empty");
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        let rep = build_sequential_with_board(
            TaskBoard::new(vec![]),
            &spy,
            "t",
            &dir,
            &base_cfg(),
            1,
            &sink,
        )
        .unwrap();
        assert!(rep.fell_back_whole_task);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dependency_cycle_still_terminates() {
        let dir = ws("cycle");
        // t1 ↔ t2 mutual deps: ready() is always empty, but the lowest-pending guard must
        // run them anyway so the loop terminates rather than hanging.
        let board = TaskBoard::new(vec![
            Subtask::new("t1", "a")
                .with_files(vec!["a.py".into()])
                .with_deps(vec!["t2".into()]),
            Subtask::new("t2", "b")
                .with_files(vec!["b.py".into()])
                .with_deps(vec!["t1".into()]),
        ]);
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        let rep =
            build_sequential_with_board(board, &spy, "t", &dir, &base_cfg(), 1, &sink).unwrap();
        // Both subtasks were attempted (≤ len iterations, no hang).
        assert_eq!(rep.per_file.len(), 2, "both attempted via the guard");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_instruction_embeds_contract_and_drops_ignore_framing() {
        // The fix: the per-file step must SEE the contract and must NOT be told to ignore it.
        let s = per_file_instruction(
            &["store.py".into()],
            "build the store",
            "def test_save(): assert save('u') is not None",
        );
        assert!(
            s.contains("`store.py`"),
            "names the file (SpyBackend parse): {s}"
        );
        assert!(
            s.contains("assert save('u')"),
            "embeds the contract asserts"
        );
        assert!(s.contains("FROZEN"), "frames the tests as the contract");
        assert!(
            !s.contains("NOT your concern") && !s.contains("no tests to run"),
            "the old ignore-the-tests framing must be gone: {s}"
        );
        // With no contract, no fenced block (degenerate/missing-test case).
        let bare = per_file_instruction(&["a.py".into()], "g", "");
        assert!(bare.contains("`a.py`") && !bare.contains("```python"));
    }

    #[test]
    fn per_file_steps_see_the_frozen_contract_from_disk() {
        // End-to-end through the driver: a test_app.py on disk reaches the per-file prompt
        // (via the glob fallback — base_cfg here has no frozen_paths).
        let dir = ws("contract");
        std::fs::write(
            dir.join("test_app.py"),
            "def test_save_returns_code():\n    assert save('u') is not None\n",
        )
        .unwrap();
        let board = TaskBoard::new(vec![
            Subtask::new("t1", "build store").with_files(vec!["store.py".into()])
        ]);
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        build_sequential_with_board(board, &spy, "task", &dir, &base_cfg(), 1, &sink).unwrap();
        let seen = spy.seen_instructions.lock().unwrap();
        assert!(
            seen.iter()
                .any(|i| i.contains("`store.py`") && i.contains("test_save_returns_code")),
            "the per-file prompt must carry the on-disk test contract"
        );
        assert!(!seen.iter().any(|i| i.contains("NOT your concern")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_frozen_contract_prefers_frozen_paths_then_globs() {
        let dir = ws("frozen-read");
        std::fs::write(dir.join("test_app.py"), "A").unwrap();
        std::fs::write(dir.join("test_more.py"), "B").unwrap();
        // Explicit frozen_paths win and are read in that order.
        let explicit = read_frozen_contract(&dir, &["test_app.py".to_string()]);
        assert_eq!(explicit, "A");
        // No frozen_paths → glob test_*.py (sorted): A then B.
        let globbed = read_frozen_contract(&dir, &[]);
        assert!(globbed.contains("A") && globbed.contains("B"));
        // Missing dir / no tests → empty.
        assert_eq!(read_frozen_contract(&ws("frozen-empty"), &[]), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_registry_has_write_but_no_verification() {
        // The per-file registry must let a step CREATE a file (write_file) but NOT have
        // run_verification (which dead-ends on the intentionally-absent verify command).
        let names: Vec<&str> = per_file_registry().specs().iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"write_file"),
            "needs write_file to create files"
        );
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"finish"));
        assert!(
            !names.contains(&"run_verification"),
            "must NOT have run_verification (the dead-end that stalled per-file steps)"
        );
        assert!(!names.contains(&"run_command"));
    }

    #[test]
    fn feature_keyword_maps_route_files_and_skips_infra() {
        assert_eq!(
            feature_keyword("routes_authors.py").as_deref(),
            Some("author")
        );
        assert_eq!(feature_keyword("routes_books.py").as_deref(), Some("book"));
        assert_eq!(feature_keyword("routes_loans.py").as_deref(), Some("loan"));
        // Infrastructure + glue are not their own feature slice.
        for f in [
            "store.py",
            "service.py",
            "app.py",
            "templates/catalog.html",
            "static/catalog.js",
            "routes.py",
        ] {
            assert_eq!(feature_keyword(f), None, "{f} should not be a slice");
        }
    }

    #[test]
    fn parse_test_names_extracts_def_test_lines() {
        let contract = "from app import app\n\ndef c():\n    return app\n\ndef test_create_author_and_book():\n    pass\n\ndef test_loan_book_ok():\n    pass\ndef test_catalog_page():\n    pass\n";
        let names = parse_test_names(contract);
        assert_eq!(
            names,
            vec![
                "test_create_author_and_book",
                "test_loan_book_ok",
                "test_catalog_page"
            ]
        );
        // `def c()` (not a test) is excluded.
        assert!(!names.iter().any(|n| n == "c"));
    }

    #[test]
    fn derive_slices_yields_features_in_dep_order_with_tests() {
        // An S3-shaped board: store→service→routes_authors→routes_books→routes_loans→app→template.
        let board = TaskBoard::new(vec![
            Subtask::new("t1", "store").with_files(vec!["store.py".into()]),
            Subtask::new("t2", "service")
                .with_files(vec!["service.py".into()])
                .with_deps(vec!["t1".into()]),
            Subtask::new("t3", "authors")
                .with_files(vec!["routes_authors.py".into()])
                .with_deps(vec!["t2".into()]),
            Subtask::new("t4", "books")
                .with_files(vec!["routes_books.py".into()])
                .with_deps(vec!["t3".into()]),
            Subtask::new("t5", "loans")
                .with_files(vec!["routes_loans.py".into()])
                .with_deps(vec!["t4".into()]),
            Subtask::new("t6", "app")
                .with_files(vec!["app.py".into()])
                .with_deps(vec!["t5".into()]),
        ]);
        let names: Vec<String> = [
            "test_create_author",
            "test_book_requires_author",
            "test_loan_book_ok",
            "test_catalog_page",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let slices = derive_slices(&board, &names);
        let kws: Vec<&str> = slices.iter().map(|s| s.keyword.as_str()).collect();
        // author/book/loan in dep order; app/store/service yield no slice; "catalog" has a test
        // but no routes_catalog.py, so no slice (the final full pass catches it).
        assert_eq!(kws, vec!["author", "book", "loan"]);
    }

    #[test]
    fn derive_slices_empty_when_no_route_files() {
        // A single-routes.py app (S1/S2 shape) → no feature slices → caller falls back.
        let board = TaskBoard::new(vec![
            Subtask::new("t1", "store").with_files(vec!["store.py".into()]),
            Subtask::new("t2", "app")
                .with_files(vec!["app.py".into()])
                .with_deps(vec!["t1".into()]),
        ]);
        let names = vec!["test_create".to_string(), "test_resolve".to_string()];
        assert!(derive_slices(&board, &names).is_empty());
    }

    #[test]
    fn cumulative_k_and_slice_command_build_growing_filters() {
        let slices = vec![
            FeatureSlice {
                keyword: "author".into(),
                file: "routes_authors.py".into(),
            },
            FeatureSlice {
                keyword: "book".into(),
                file: "routes_books.py".into(),
            },
            FeatureSlice {
                keyword: "loan".into(),
                file: "routes_loans.py".into(),
            },
        ];
        assert_eq!(cumulative_k(&slices, 0), "author");
        assert_eq!(cumulative_k(&slices, 1), "author or book");
        assert_eq!(cumulative_k(&slices, 2), "author or book or loan");
        assert_eq!(
            slice_command("python -m pytest -q 'test_app.py'", "author or book"),
            "python -m pytest -q 'test_app.py' -k \"author or book\""
        );
    }

    #[test]
    fn incremental_integration_runs_slices_in_order_then_full_pass() {
        // With route files + matching tests + a verify command, the driver runs each cumulative
        // slice (author, author or book, author or book or loan) THEN the full pass. We use an
        // always-failing verify command so each slice pre-check is red (the agent loop runs and
        // records its sliced instruction) — we assert the ORDER of instructions, not greenness.
        let dir = ws("incr");
        let board = TaskBoard::new(vec![
            Subtask::new("t1", "authors").with_files(vec!["routes_authors.py".into()]),
            Subtask::new("t2", "books")
                .with_files(vec!["routes_books.py".into()])
                .with_deps(vec!["t1".into()]),
            Subtask::new("t3", "loans")
                .with_files(vec!["routes_loans.py".into()])
                .with_deps(vec!["t2".into()]),
        ]);
        // Frozen contract drives parse_test_names; write it so read_frozen_contract finds it.
        std::fs::write(
            dir.join("test_app.py"),
            "def test_author():\n    pass\ndef test_book():\n    pass\ndef test_loan():\n    pass\n",
        ).unwrap();
        let mut cfg = AgentConfig {
            // An unknown program → shell exits non-zero → not all_green → each slice loop runs.
            verify_command: Some("dc_nonexistent_verify_cmd_xyz".to_string()),
            ..AgentConfig::default()
        };
        cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        let rep = build_sequential_with_board(board, &spy, "lib", &dir, &cfg, 1, &sink).unwrap();

        // The incremental steps were recorded in cumulative order.
        let labels: Vec<&str> = rep.incremental.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "slice:author",
                "slice:author or book",
                "slice:author or book or loan"
            ]
        );
        // The model saw the sliced instructions in order, then the full-suite pass last.
        let seen = spy.seen_instructions.lock().unwrap();
        let slice_positions: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, i)| i.contains("GROWING SLICE"))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(slice_positions.len() >= 3, true, "ran the slice loops");
        let last = seen.last().unwrap();
        assert!(
            last.contains("Make the FULL frozen test suite pass"),
            "full pass is last: {last}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn final_pass_runs_unfocused_after_the_per_file_steps() {
        let dir = ws("final");
        let board = TaskBoard::new(vec![Subtask::new("t1", "a").with_files(vec!["a.py".into()])]);
        let spy = SpyBackend::new();
        let sink = FnSink(|_e: &sc_core::AgentEvent| {});
        let _ = build_sequential_with_board(board, &spy, "the task", &dir, &base_cfg(), 1, &sink)
            .unwrap();
        // The LAST instruction the model saw is the integration pass, not a per-file one.
        let seen = spy.seen_instructions.lock().unwrap();
        let last = seen.last().unwrap();
        assert!(
            last.contains("Make the FULL frozen test suite pass"),
            "final pass is the integration instruction: {last}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
