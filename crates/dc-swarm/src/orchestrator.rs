//! The orchestrator (spec 08): decompose → schedule parallel workers → integrate
//! their proposals one at a time with verification.
//!
//! Concurrency posture (spec 08): **parallel intelligence, serialized writes.**
//! Independent subtasks run as concurrent workers, each in its own scratch copy
//! (the slow reasoning happens in parallel). Their proposed changes are then
//! applied to the real workspace **one at a time**, each gated by verification —
//! so the mainline always has a single coherent state, and a proposal that breaks
//! the suite is reverted, never landed.

use std::path::Path;
use std::sync::Mutex;

use dc_core::AgentConfig;
use dc_model::ModelBackend;

use crate::decompose::decompose_observed;
use crate::event::{SwarmEvent, SwarmSink};
use crate::worker::{propose_prompt_with_feedback, run_worker, ProposedChange, WorkerResult};

/// Configuration for a swarm run.
#[derive(Debug, Clone)]
pub struct SwarmConfig {
    /// Max workers running at once (bounded by hardware, spec 08).
    pub max_workers: usize,
    /// The per-worker agent-loop config (budgets, verify command, etc.).
    pub worker: AgentConfig,
    /// The verification command run after each integration (whole-suite gate). If
    /// `None`, proposals are accepted without an integration check.
    pub verify_command: Option<String>,
    /// Frozen contract-test paths (spec 11): the integration merge will NEVER write
    /// to these, so workers make the tests pass instead of weakening them. Set by
    /// the staged workflow from the tests it wrote in Phase 4.
    pub frozen_paths: Vec<String>,
    /// Per-subtask retry cap (spec 08 — "Subtask retry on partial or rejected
    /// integration"). When an accepted-but-incomplete (or rejected) proposal leaves
    /// a subtask's scoped tests red, the orchestrator re-dispatches the subtask to a
    /// worker with failing-test feedback, up to this many extra attempts. Total
    /// worker invocations for a subtask is `1 + max_subtask_retries`. `0` restores
    /// the no-retry behaviour. Default **2**.
    pub max_subtask_retries: usize,
    /// Where the verify command runs (spec 12): the host, or a per-run ephemeral Docker
    /// container. Docker gives generated code a pinned toolkit + a known layout, so a
    /// build doesn't depend on (or pollute) the host. Defaults to [`Sandbox::Host`].
    pub sandbox: dc_verify::Sandbox,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_workers: 2,
            worker: AgentConfig::default(),
            verify_command: None,
            frozen_paths: Vec::new(),
            max_subtask_retries: 2,
            sandbox: dc_verify::Sandbox::default(),
        }
    }
}

/// The outcome of a swarm run.
#[derive(Debug, Clone)]
pub struct SwarmReport {
    /// done / failed / pending subtask counts.
    pub done: usize,
    pub failed: usize,
    pub pending: usize,
    /// Whether every subtask completed and integrated.
    pub all_done: bool,
    /// Files changed in the real workspace, accepted via integration.
    pub integrated_files: Vec<String>,
}

/// Run the swarm: orchestrate `task` over `worker_backend` workers (and an
/// optional `advisor`), decomposing with `orchestrator`, against `workspace`.
#[allow(clippy::too_many_arguments)]
pub fn run_swarm(
    orchestrator: &dyn ModelBackend,
    worker_backend: &(dyn ModelBackend + Sync),
    advisor: Option<&(dyn ModelBackend + Sync)>,
    task: &str,
    repo_overview: &str,
    workspace: &Path,
    cfg: &SwarmConfig,
    sink: &dyn SwarmSink,
) -> SwarmReport {
    let d = decompose_observed(orchestrator, task, repo_overview);
    // Surface the decomposition prompt + raw reply before the board, so a UI can show
    // what the orchestrator was asked and answered (and whether it fell back).
    sink.record(&SwarmEvent::OrchestratorPrompt {
        prompt: d.prompt,
        reply: d.reply,
        fell_back: d.fell_back,
    });
    run_swarm_board(
        orchestrator,
        worker_backend,
        advisor,
        d.board,
        workspace,
        cfg,
        sink,
    )
}

/// Run the swarm against a **pre-built** task board (spec 09 → 08): when the
/// staged workflow already decomposed the work, the swarm executes that board
/// directly instead of re-decomposing from a task string.
#[allow(clippy::too_many_arguments)]
pub fn run_swarm_board(
    orchestrator: &dyn ModelBackend,
    worker_backend: &(dyn ModelBackend + Sync),
    advisor: Option<&(dyn ModelBackend + Sync)>,
    mut board: crate::board::TaskBoard,
    workspace: &Path,
    cfg: &SwarmConfig,
    sink: &dyn SwarmSink,
) -> SwarmReport {
    sink.record(&SwarmEvent::Decomposed {
        subtasks: board.subtasks().iter().map(|s| s.goal.clone()).collect(),
    });

    let mut integrated_files: Vec<String> = Vec::new();

    // Schedule in waves: each wave runs the currently-ready (independent)
    // subtasks in parallel, then integrates their proposals serially.
    while !board.is_quiescent() {
        let ready = board.ready();
        if ready.is_empty() {
            break;
        }

        // Take up to max_workers ready subtasks for this wave.
        let wave: Vec<crate::board::Subtask> = ready
            .iter()
            .take(cfg.max_workers.max(1))
            .filter_map(|id| board.subtasks().iter().find(|s| &s.id == id).cloned())
            .collect();
        for st in &wave {
            board.claim(&st.id);
            sink.record(&SwarmEvent::WorkerStarted {
                subtask: st.id.clone(),
                goal: st.goal.clone(),
                // The exact single-shot prompt this coder is handed (first attempt, no
                // feedback) — what the worker "sees", surfaced for the UI.
                prompt: propose_prompt_with_feedback(st, workspace, None),
            });
        }

        // Run the wave's workers in parallel (the slow part), collecting results.
        let results = Mutex::new(Vec::<WorkerResult>::new());
        std::thread::scope(|scope| {
            for st in &wave {
                let results = &results;
                let st = st.clone();
                let wcfg = cfg.worker.clone();
                scope.spawn(move || {
                    // Coerce the Sync trait objects to plain &dyn ModelBackend for
                    // the worker (which doesn't require Sync itself).
                    let wb: &dyn ModelBackend = worker_backend;
                    let adv: Option<&dyn ModelBackend> = advisor.map(|a| a as &dyn ModelBackend);
                    let r = run_worker(wb, adv, &st, workspace, &wcfg);
                    results.lock().unwrap().push(r);
                });
            }
        });
        let mut results = results.into_inner().unwrap();
        // Deterministic integration order: by subtask id.
        results.sort_by(|a, b| a.subtask_id.cmp(&b.subtask_id));

        // Integrate proposals ONE AT A TIME, verifying after each (serialized). Each
        // subtask runs through the scoped retry loop (spec 08): integrate → check the
        // subtask's OWN tests → on incomplete, re-dispatch with feedback up to
        // `max_subtask_retries`.
        for result in results {
            integrate_with_retry(
                orchestrator,
                worker_backend,
                advisor,
                &wave,
                workspace,
                result,
                cfg,
                sink,
                &mut board,
                &mut integrated_files,
            );
        }
    }

    let (done, failed, pending) = board.tally();
    // Final integration verification (spec 08 step 5: "Only after integration
    // verification passes does the orchestrator finish"). The per-merge gate only
    // checks "didn't make it worse" — a worker's *partial* fix can keep the failing
    // count flat, integrate, and leave the board all-done over a still-red suite.
    // Re-run the whole suite once at the end so "done" means the mainline is
    // actually green, not merely that every subtask landed (honest stop, spec 06).
    let all_done = board.all_done()
        && match &cfg.verify_command {
            Some(cmd) => {
                badness(&dc_verify::run_verification_in(
                    &cfg.sandbox,
                    workspace,
                    cmd,
                )) == 0
            }
            None => true,
        };
    sink.record(&SwarmEvent::SwarmDone {
        done,
        failed,
        all_done,
    });
    SwarmReport {
        done,
        failed,
        pending,
        all_done,
        integrated_files,
    }
}

enum Integration {
    Accepted(Vec<String>),
    Rejected(String),
}

/// Integrate one worker result and, if the subtask's own tests aren't satisfied,
/// retry it with feedback up to `max_subtask_retries` (spec 08 — "Subtask retry on
/// partial or rejected integration"). This layers a **scoped, per-subtask completion
/// check** on top of the existing cumulative whole-suite gate: a subtask is `Done`
/// only when (a) the merge didn't worsen the suite AND (b) its own tests pass.
///
/// On exhaustion the subtask is marked `Failed` with the residual failures as the
/// reason; dependents then block via the board's quiescence rule. Every attempt is
/// gated and serialized exactly like the first (a regressing retry is reverted by
/// `integrate` itself).
#[allow(clippy::too_many_arguments)]
fn integrate_with_retry(
    orchestrator: &dyn ModelBackend,
    worker_backend: &(dyn ModelBackend + Sync),
    advisor: Option<&(dyn ModelBackend + Sync)>,
    wave: &[crate::board::Subtask],
    workspace: &Path,
    mut result: WorkerResult,
    cfg: &SwarmConfig,
    sink: &dyn SwarmSink,
    board: &mut crate::board::TaskBoard,
    integrated_files: &mut Vec<String>,
) {
    let id = result.subtask_id.clone();
    let subtask = wave.iter().find(|s| s.id == id).cloned();

    // The whole-suite baseline for THIS subtask's free-text fallback: the failing
    // count just before this subtask's first merge (so the fallback can ask "did this
    // subtask's merge clear the suite?"). Only needed when frozen tests are unknown.
    let baseline = match (&cfg.verify_command, cfg.frozen_paths.is_empty()) {
        (Some(cmd), true) => Some(badness(&dc_verify::run_verification_in(
            &cfg.sandbox,
            workspace,
            cmd,
        ))),
        _ => None,
    };

    let mut attempt = 0usize;
    loop {
        sink.record(&SwarmEvent::WorkerFinished {
            subtask: id.clone(),
            summary: result.report_summary.clone(),
            proposal: result.proposal.clone(),
        });

        let outcome = integrate(orchestrator, workspace, &result, cfg);
        let accepted_files = match &outcome {
            Integration::Accepted(files) => Some(files.clone()),
            Integration::Rejected(_) => None,
        };

        // Decide the subtask's TRUE status from a scoped check, not the cumulative
        // gate alone (spec 08). A rejected merge is trivially incomplete.
        let residual: Vec<dc_verify::TestCase> = match &cfg.verify_command {
            // `max_subtask_retries == 0` restores today's behaviour (spec 08): no
            // scoped completion check, no retry — trust the cumulative gate alone.
            // Accept → Done (the final whole-suite verify is the only backstop).
            _ if cfg.max_subtask_retries == 0 && accepted_files.is_some() => Vec::new(),
            // No verify command: nothing to scope against. An accepted merge is taken
            // as complete; a rejected one is incomplete.
            None if accepted_files.is_some() => Vec::new(),
            None => vec![synthetic_failure("integration rejected")],
            // With a verify command, scope to the subtask's own tests — for both an
            // accept (is the partial fix actually complete?) and a reject (the merge
            // was reverted; check the prior state against this subtask's contract).
            Some(cmd) => scoped_failures(&cfg.sandbox, workspace, cmd, &cfg.frozen_paths, baseline),
        };

        if let (true, Some(files)) = (residual.is_empty(), accepted_files) {
            // Genuinely done: the gate accepted AND the subtask's tests are green.
            board.complete(&id);
            for f in &files {
                if !integrated_files.contains(f) {
                    integrated_files.push(f.clone());
                }
            }
            sink.record(&SwarmEvent::Integrated {
                subtask: id.clone(),
                accepted: true,
                files,
            });
            return;
        }

        // Incomplete (accepted-but-partial, or rejected). Retry if budget remains and
        // we know the subtask to re-dispatch.
        let failing: Vec<String> = residual.iter().map(|c| c.name.clone()).collect();
        if attempt < cfg.max_subtask_retries {
            if let Some(st) = &subtask {
                attempt += 1;
                sink.record(&SwarmEvent::SubtaskRetry {
                    subtask: id.clone(),
                    attempt,
                    max: cfg.max_subtask_retries,
                    failing_tests: failing.clone(),
                });
                let mut feedback = feedback_text(&residual);

                // Before the FINAL retry, escalate to the advisor for a one-line nudge
                // ("junior asks senior", spec 02/08) — advice, not the fix. We fold the
                // hint into this last attempt's prompt. Only on the final attempt (so a
                // cheap subtask that recovers early never pays the senior call), and only
                // if an advisor is configured.
                let is_final = attempt == cfg.max_subtask_retries;
                if is_final {
                    if let Some(adv) = advisor {
                        let predicament = dc_core::Predicament {
                            task: &st.goal,
                            plan: &format!("subtask {id}: {}", st.goal),
                            recent: &result.report_summary,
                            trigger: &format!("scoped tests still failing: {}", failing.join(", ")),
                        };
                        if let Some(advice) = dc_core::consult(adv, &predicament) {
                            sink.record(&SwarmEvent::AdvisorConsulted {
                                subtask: id.clone(),
                                advice: advice.clone(),
                            });
                            feedback.push('\n');
                            feedback.push_str(&dc_core::advice_observation(&advice));
                        }
                    }
                }

                let adv: Option<&dyn ModelBackend> = advisor.map(|a| a as &dyn ModelBackend);
                let wb: &dyn ModelBackend = worker_backend;
                result = crate::worker::run_worker_with_feedback(
                    wb,
                    adv,
                    st,
                    workspace,
                    &cfg.worker,
                    Some(&feedback),
                );
                continue;
            }
        }

        // Exhausted (or nothing to re-dispatch): mark Failed with the residual as the
        // reason (spec 08 — Failed, not Done; dependents block via quiescence).
        board.fail(&id);
        let reason = if failing.is_empty() {
            match &outcome {
                Integration::Rejected(r) => r.clone(),
                Integration::Accepted(_) => "subtask tests still failing".to_string(),
            }
        } else {
            format!("subtask tests still failing: {}", failing.join(", "))
        };
        sink.record(&SwarmEvent::Integrated {
            subtask: id.clone(),
            accepted: false,
            files: vec![reason],
        });
        return;
    }
}

/// The subtask's residual failing tests after a merge — the *scoped* completion
/// check (spec 08 step 1). Two modes:
///
/// - **Frozen tests known** (staged workflow): run the verify command **filtered to
///   the frozen contract-test paths** (`pytest <those files>`); the failing cases it
///   reports are the subtask's own unmet contract. Precise.
/// - **Frozen tests unknown** (free-text `swarm <task>`, `frozen_paths` empty): fall
///   back to the **whole-suite delta vs. this subtask's baseline** — incomplete iff
///   the suite is still red AND this subtask's merge didn't clear it. Coarser (can't
///   attribute a residual to one subtask), but stops a red run being called done.
fn scoped_failures(
    sandbox: &dc_verify::Sandbox,
    workspace: &Path,
    verify_command: &str,
    frozen: &[String],
    baseline: Option<usize>,
) -> Vec<dc_verify::TestCase> {
    if frozen.is_empty() {
        // Free-text fallback: whole-suite delta vs. this subtask's own baseline.
        let report = dc_verify::run_verification_in(sandbox, workspace, verify_command);
        let after = badness(&report);
        let still_red = after > 0;
        let cleared = baseline.map(|b| after < b).unwrap_or(false);
        if still_red && !cleared {
            let failed: Vec<dc_verify::TestCase> = report.failed().into_iter().cloned().collect();
            if failed.is_empty() {
                vec![synthetic_failure("suite still red after this subtask")]
            } else {
                failed
            }
        } else {
            Vec::new()
        }
    } else {
        // Precise: verify filtered to the frozen contract tests for this subtask.
        let cmd = format!("{verify_command} {}", frozen.join(" "));
        let report = dc_verify::run_verification_in(sandbox, workspace, &cmd);
        if report.generic {
            // No per-test breakdown — fall back to the command's pass/fail.
            if report.command_ok {
                Vec::new()
            } else {
                vec![synthetic_failure(
                    "scoped tests failed (no per-test detail)",
                )]
            }
        } else {
            report.failed().into_iter().cloned().collect()
        }
    }
}

/// A stand-in failing case for paths where we know the subtask is incomplete but have
/// no per-test breakdown (generic/exit-code-only suites, rejected merges).
fn synthetic_failure(msg: &str) -> dc_verify::TestCase {
    dc_verify::TestCase {
        name: msg.to_string(),
        passed: false,
        message: None,
    }
}

/// The feedback block for a retry prompt: still-failing test names + their assertion
/// messages (spec 08 — `TestReport::failed()` carries `name` + `message`).
fn feedback_text(residual: &[dc_verify::TestCase]) -> String {
    let mut s = String::new();
    for c in residual {
        s.push_str(&format!("✗ {}", c.name));
        if let Some(m) = &c.message {
            s.push_str(&format!("\n    {}", m.replace('\n', "\n    ")));
        }
        s.push('\n');
    }
    s.trim_end().to_string()
}

/// How "bad" a verification result is, comparable before vs after a change. For a
/// parsed report it's the number of failing tests, plus one if the command itself
/// errored with no failures parsed (e.g. a pytest *collection* error from a broken
/// import — green-looking to a naive failed-count but actually a hard failure). For
/// a generic (exit-code-only) report it's 0 if the command passed, else 1. This
/// lets the cumulative gate ("don't make it worse") work for both pytest-style and
/// bare-shell suites and never mistake a collection error for success.
fn badness(report: &dc_verify::TestReport) -> usize {
    if report.generic {
        usize::from(!report.command_ok)
    } else {
        let failures = report.failed().len();
        // A non-zero exit with zero parsed failures means the suite didn't even run
        // (import/collection error) — count it as bad so the gate won't accept it.
        failures + usize::from(failures == 0 && !report.command_ok)
    }
}

/// Is `path` one of the frozen contract-test paths? Compared with normalized
/// separators so `tests/a.py` and `tests\a.py` match.
fn is_frozen(path: &str, frozen: &[String]) -> bool {
    let norm = |s: &str| s.replace('\\', "/");
    let p = norm(path);
    frozen.iter().any(|f| norm(f) == p)
}

/// Merge a worker's *text* proposal into the real workspace, then verify (spec 08
/// — parallel reasoning, serialized & reviewed writes).
///
/// The tiny worker handed back its fix as text; the smarter `orchestrator` turns
/// that into the actual file. For each focused file it asks the orchestrator to
/// produce the complete corrected file (reviewing the worker's proposal against
/// the real current contents), writes it, then runs the whole suite. A merge that
/// breaks the suite is reverted and rejected — the mainline stays coherent.
fn integrate(
    orchestrator: &dyn ModelBackend,
    workspace: &Path,
    result: &WorkerResult,
    cfg: &SwarmConfig,
) -> Integration {
    if !result.has_proposal() {
        return Integration::Rejected("no proposal from worker".to_string());
    }
    if result.files.is_empty() {
        return Integration::Rejected("proposal has no target file".to_string());
    }
    // A subtask that targets ONLY frozen contract tests has nothing to do — workers
    // make the tests pass, they don't rewrite them (spec 11).
    if result.files.iter().all(|f| is_frozen(f, &cfg.frozen_paths)) {
        return Integration::Rejected("subtask targets only frozen contract tests".to_string());
    }

    // Ask the orchestrator to turn the proposal into the corrected file(s). Frozen
    // contract tests are skipped — the merge may never overwrite them.
    let mut changes = Vec::new();
    for file in &result.files {
        if is_frozen(file, &cfg.frozen_paths) {
            continue;
        }
        let current = std::fs::read_to_string(workspace.join(file))
            .unwrap_or_default()
            .replace("\r\n", "\n");
        match merge_file(orchestrator, file, &current, &result.proposal) {
            Some(merged) if merged != current => changes.push(ProposedChange {
                path: file.clone(),
                after: Some(merged),
            }),
            _ => {}
        }
    }
    if changes.is_empty() {
        return Integration::Rejected("orchestrator produced no change".to_string());
    }

    // Snapshot the files we're about to touch so we can revert on rejection.
    let backup: Vec<(String, Option<String>)> = changes
        .iter()
        .map(|c| {
            let p = workspace.join(&c.path);
            (c.path.clone(), std::fs::read_to_string(&p).ok())
        })
        .collect();

    // No verify command: nothing to gate on, just apply.
    let Some(cmd) = &cfg.verify_command else {
        apply_changes(workspace, &changes);
        return Integration::Accepted(changes.iter().map(|c| c.path.clone()).collect());
    };

    // Baseline failure count BEFORE applying, so a multi-file task can land its
    // pieces cumulatively. A subtask that fixes only its own file leaves the whole
    // suite red (other files still broken) — but it must not be reverted for that.
    // The gate is "didn't make things worse": accept if the failing-test count goes
    // down or stays equal; reject only a change that increases failures. The run is
    // "done" only when every subtask lands and the board is all-done — by which
    // point, for genuine fixes, the suite is actually green.
    let before = badness(&dc_verify::run_verification_in(
        &cfg.sandbox,
        workspace,
        cmd,
    ));
    apply_changes(workspace, &changes);
    let after = badness(&dc_verify::run_verification_in(
        &cfg.sandbox,
        workspace,
        cmd,
    ));

    if after <= before {
        Integration::Accepted(changes.iter().map(|c| c.path.clone()).collect())
    } else {
        revert(workspace, &backup);
        Integration::Rejected(format!(
            "broke the suite at integration ({before} -> {after} failing)"
        ))
    }
}

/// Ask the orchestrator to apply `proposal` to `current`, returning the complete
/// corrected file. A single call (the fastest merge) — the capable model handles
/// the exact reproduction the tiny worker couldn't. `None` if it errored.
fn merge_file(
    orchestrator: &dyn ModelBackend,
    path: &str,
    current: &str,
    proposal: &str,
) -> Option<String> {
    use dc_model::{GenerateRequest, Message};
    // `/no_think` for the same reason as the proposer (worker.rs): a Qwen3-class
    // orchestrator otherwise writes its reasoning into the merged file. Merge only
    // ever wants the final file bytes.
    let system = "You apply a proposed fix to a file. You are given the CURRENT file \
        and a worker's proposed corrected version. Output the complete, final file \
        contents only — no markdown fences, no commentary. Keep everything the fix \
        doesn't change; apply the fix exactly. /no_think";
    let user = format!(
        "File: {path}\n\n--- CURRENT ---\n{current}\n\n--- PROPOSED FIX ---\n{proposal}\n\n\
         Output the complete corrected {path}:"
    );
    let req = GenerateRequest::new(vec![Message::system(system), Message::user(user)]);
    let raw = orchestrator.generate(&req).ok()?.content;
    Some(unfence(&raw))
}

/// Strip a surrounding ``` fence (optional language tag) the model may add, then
/// ensure exactly one trailing newline (normal for a source file). Without a fence
/// the body is preserved as-is (aside from the trailing newline).
fn unfence(s: &str) -> String {
    let trimmed = s.trim_start();
    let body = if let Some(rest) = trimmed.strip_prefix("```") {
        // Drop the ``` (or ```lang) line and a trailing ``` fence.
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        rest.trim_end()
            .strip_suffix("```")
            .unwrap_or(rest)
            .trim_end()
            .to_string()
    } else {
        s.trim_end().to_string()
    };
    format!("{body}\n")
}

fn apply_changes(workspace: &Path, changes: &[ProposedChange]) {
    for c in changes {
        let p = workspace.join(&c.path);
        match &c.after {
            Some(content) => {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&p, content);
            }
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

fn revert(workspace: &Path, backup: &[(String, Option<String>)]) {
    for (rel, content) in backup {
        let p = workspace.join(rel);
        match content {
            Some(c) => {
                let _ = std::fs::write(&p, c);
            }
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NullSwarmSink;
    use dc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
    use dc_proto::Result;
    use std::sync::Mutex as StdMutex;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-swarm-orch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A backend that maps a per-subtask script: it inspects the instruction to
    /// decide what to emit. Thread-safe (Sync) so workers can share it.
    struct ScriptedSwarm {
        // instruction-substring -> queued replies
        scripts: StdMutex<Vec<(String, Vec<String>)>>,
    }
    impl ScriptedSwarm {
        fn new(scripts: Vec<(&str, Vec<&str>)>) -> Self {
            Self {
                scripts: StdMutex::new(
                    scripts
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
                        .collect(),
                ),
            }
        }
    }
    impl ModelBackend for ScriptedSwarm {
        fn name(&self) -> &str {
            "scripted-swarm"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
            let instr = req
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let mut scripts = self.scripts.lock().unwrap();
            for (key, queue) in scripts.iter_mut() {
                if instr.contains(key.as_str()) && !queue.is_empty() {
                    return Ok(GenerateResponse {
                        content: queue.remove(0),
                    });
                }
            }
            Ok(GenerateResponse {
                content: r#"{"tool":"finish"}"#.to_string(),
            })
        }
    }

    #[test]
    fn two_independent_subtasks_propose_and_merge() {
        let ws = temp("two");
        std::fs::write(ws.join("a.txt"), "old-a").unwrap();
        std::fs::write(ws.join("b.txt"), "old-b").unwrap();

        // Flow per subtask: orchestrator decomposes -> tiny worker PROPOSES the
        // corrected file as text -> orchestrator MERGES the proposal into the file.
        // The merge prompt contains "--- CURRENT ---" (the proposer's doesn't), so we
        // key the merge replies on that and the proposer replies on the goal.
        let backend = ScriptedSwarm::new(vec![
            // decomposition
            (
                "Break the coding task",
                vec![
                    r#"[{"id":"a","goal":"set a.txt to new-a","files":["a.txt"]},{"id":"b","goal":"set b.txt to new-b","files":["b.txt"]}]"#,
                ],
            ),
            // Merge calls (orchestrator) — prompt contains "File: <path>"; key on that.
            ("File: a.txt", vec!["new-a"]),
            ("File: b.txt", vec!["new-b"]),
            // Proposer calls (worker) — prompt contains the goal; key on that.
            ("set a.txt to new-a", vec!["new-a"]),
            ("set b.txt to new-b", vec!["new-b"]),
        ]);

        let report = run_swarm(
            &backend,
            &backend,
            None,
            "update a and b",
            "",
            &ws,
            &SwarmConfig::default(),
            &NullSwarmSink,
        );

        assert!(
            report.all_done,
            "both subtasks should integrate: {report:?}"
        );
        assert_eq!(report.done, 2);
        // The merge normalizes to a single trailing newline.
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "new-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("b.txt")).unwrap(),
            "new-b\n"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_merge_that_breaks_the_suite_is_rejected_and_reverted() {
        let ws = temp("reject");
        // A working impl + a frozen pytest that passes for it. Python (not `sh`) so
        // the verify command is portable across platforms (incl. Windows CI).
        std::fs::write(
            ws.join("calc.py"),
            "def is_even(n):\n    return n % 2 == 0\n",
        )
        .unwrap();
        std::fs::write(
            ws.join("test_calc.py"),
            "from calc import is_even\n\n\ndef test_even():\n    assert is_even(4)\n",
        )
        .unwrap();

        // The worker proposes a broken impl; the orchestrator merges it; the suite
        // goes red, so the merge is reverted and the subtask fails.
        let broken = "def is_even(n):\n    return False\n";
        let backend = ScriptedSwarm::new(vec![
            (
                "Break the coding task",
                vec![r#"[{"id":"x","goal":"break calc.py badly","files":["calc.py"]}]"#],
            ),
            // merge (keyed on "File: <path>") and proposer (keyed on goal) both yield
            // the broken version.
            ("File: calc.py", vec![broken]),
            ("break calc.py badly", vec![broken]),
        ]);

        let cfg = SwarmConfig {
            verify_command: Some("python -m pytest -q".to_string()),
            ..Default::default()
        };
        let report = run_swarm(
            &backend,
            &backend,
            None,
            "break it",
            "",
            &ws,
            &cfg,
            &NullSwarmSink,
        );

        assert!(!report.all_done);
        assert_eq!(report.failed, 1);
        // calc.py was reverted to the working version (integration rejected it).
        let impl_after = std::fs::read_to_string(ws.join("calc.py")).unwrap();
        assert!(
            impl_after.contains("n % 2 == 0"),
            "should be reverted: {impl_after}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_merge_never_overwrites_a_frozen_contract_test() {
        let ws = temp("frozen");
        std::fs::write(ws.join("test_it.py"), "FROZEN CONTRACT\n").unwrap();
        std::fs::write(ws.join("impl.py"), "old\n").unwrap();

        // One subtask whose worker proposes to rewrite BOTH impl.py and the frozen
        // test. The merge applies impl.py but must leave the test untouched.
        let backend = ScriptedSwarm::new(vec![
            (
                "Break the coding task",
                vec![r#"[{"id":"x","goal":"do it","files":["impl.py","test_it.py"]}]"#],
            ),
            ("do it", vec!["new impl"]),
            ("File: impl.py", vec!["new\n"]),
            ("File: test_it.py", vec!["HACKED\n"]),
        ]);

        let cfg = SwarmConfig {
            frozen_paths: vec!["test_it.py".to_string()],
            ..Default::default()
        };
        let _ = run_swarm_board(
            &backend,
            &backend,
            None,
            crate::board::TaskBoard::new(vec![crate::board::Subtask::new("x", "do it")
                .with_files(vec!["impl.py".into(), "test_it.py".into()])]),
            &ws,
            &cfg,
            &NullSwarmSink,
        );

        // The frozen test is byte-for-byte intact; impl.py got the merge.
        assert_eq!(
            std::fs::read_to_string(ws.join("test_it.py")).unwrap(),
            "FROZEN CONTRACT\n"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
