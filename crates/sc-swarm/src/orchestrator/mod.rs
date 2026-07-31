//! The orchestrator (spec 08): decompose → schedule parallel workers → integrate
//! their proposals one at a time with verification.
//!
//! Concurrency posture (spec 08): **parallel intelligence, serialized writes.**
//! Independent subtasks run as concurrent workers, each in its own scratch copy
//! (the slow reasoning happens in parallel). Their proposed changes are then
//! applied to the real workspace **one at a time**, each gated by verification —
//! so the mainline always has a single coherent state, and a proposal that breaks
//! the suite is reverted, never landed.
//!
//! Split into three parts: the scheduling loop here, [`merge`] for turning a
//! proposal into files on disk, and [`scope`] for deciding whether one subtask is
//! actually finished.

mod merge;
mod scope;

use std::path::Path;
use std::sync::Mutex;

use sc_core::AgentConfig;
use sc_model::ModelBackend;

use crate::decompose::decompose_observed;
use crate::event::{SwarmEvent, SwarmSink};
use crate::worker::{propose_prompt_with_feedback, run_worker, WorkerResult};

use merge::integrate;
use scope::{badness, feedback_text, own_tests, scoped_failures, synthetic_failure};

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
    pub sandbox: sc_verify::Sandbox,
    /// Post-integration review (spec 16): a second gate over the integrated diff,
    /// asking *should this code stay?* after verification has answered *does it
    /// work?*. **Off by default** — it is model calls a user must opt into paying
    /// for, and a suggestion that halts a run is a tool that gets switched off.
    pub review: sc_review::ReviewConfig,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_workers: 2,
            worker: AgentConfig::default(),
            verify_command: None,
            frozen_paths: Vec::new(),
            max_subtask_retries: 2,
            sandbox: sc_verify::Sandbox::default(),
            review: sc_review::ReviewConfig::default(),
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
    /// Review findings that were never resolved, per subtask (spec 16). A subtask
    /// that passes its tests but fails review on its last allowed attempt is
    /// `Done` **with findings attached** — the work is verified correct, and
    /// throwing away green integrated code over an unfixed finding is the worse
    /// outcome. Reported loudly rather than dropped.
    pub findings: Vec<(String, Vec<sc_review::Finding>)>,
    /// Findings that met the gating severity while corroborated — the count that
    /// says a human should look before this run is called done.
    pub blocking_findings: usize,
    /// The run stopped at a review checkpoint rather than running out of work
    /// (spec 16 — "Gate"). Everything already integrated stayed integrated; the
    /// remaining subtasks are `pending`. Carried so a surface can say *why* a run
    /// with pending work ended, instead of reporting it as a failure.
    pub stopped_at_checkpoint: bool,
}

/// Run the swarm: orchestrate `task` over `worker_backend` workers (and an
/// optional `advisor`), decomposing with `orchestrator`, against `workspace`.
///
/// Headless: review findings that meet the gating bar are reported loudly but
/// never stop the run. Use [`run_swarm_gated`] to supply a human checkpoint.
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
    run_swarm_gated(
        orchestrator,
        worker_backend,
        advisor,
        task,
        repo_overview,
        workspace,
        cfg,
        sink,
        &crate::review::AutoContinue,
    )
}

/// [`run_swarm`] with a human checkpoint for gating review findings (spec 16).
#[allow(clippy::too_many_arguments)]
pub fn run_swarm_gated(
    orchestrator: &dyn ModelBackend,
    worker_backend: &(dyn ModelBackend + Sync),
    advisor: Option<&(dyn ModelBackend + Sync)>,
    task: &str,
    repo_overview: &str,
    workspace: &Path,
    cfg: &SwarmConfig,
    sink: &dyn SwarmSink,
    gate: &dyn crate::review::ReviewGate,
) -> SwarmReport {
    let d = decompose_observed(orchestrator, task, repo_overview);
    // Surface the decomposition prompt + raw reply before the board, so a UI can show
    // what the orchestrator was asked and answered (and whether it fell back).
    sink.record(&SwarmEvent::OrchestratorPrompt {
        prompt: d.prompt,
        reply: d.reply,
        fell_back: d.fell_back,
    });
    run_swarm_board_gated(
        orchestrator,
        worker_backend,
        advisor,
        d.board,
        workspace,
        cfg,
        sink,
        gate,
    )
}

/// Run the swarm against a **pre-built** task board (spec 09 → 08): when the
/// staged workflow already decomposed the work, the swarm executes that board
/// directly instead of re-decomposing from a task string.
///
/// Headless: review findings that meet the gating bar are reported loudly but
/// never stop the run. Use [`run_swarm_board_gated`] to supply a human checkpoint.
#[allow(clippy::too_many_arguments)]
pub fn run_swarm_board(
    orchestrator: &dyn ModelBackend,
    worker_backend: &(dyn ModelBackend + Sync),
    advisor: Option<&(dyn ModelBackend + Sync)>,
    board: crate::board::TaskBoard,
    workspace: &Path,
    cfg: &SwarmConfig,
    sink: &dyn SwarmSink,
) -> SwarmReport {
    run_swarm_board_gated(
        orchestrator,
        worker_backend,
        advisor,
        board,
        workspace,
        cfg,
        sink,
        &crate::review::AutoContinue,
    )
}

/// [`run_swarm_board`] with a human checkpoint for gating review findings
/// (spec 16 — "Gate"). `gate` is consulted only when review is in `gate` mode and
/// a *corroborated* finding met the configured severity; it is never consulted for
/// an opinion, however severe or however many reviewers raised it.
#[allow(clippy::too_many_arguments)]
pub fn run_swarm_board_gated(
    orchestrator: &dyn ModelBackend,
    worker_backend: &(dyn ModelBackend + Sync),
    advisor: Option<&(dyn ModelBackend + Sync)>,
    mut board: crate::board::TaskBoard,
    workspace: &Path,
    cfg: &SwarmConfig,
    sink: &dyn SwarmSink,
    gate: &dyn crate::review::ReviewGate,
) -> SwarmReport {
    sink.record(&SwarmEvent::Decomposed {
        subtasks: board.subtasks().iter().map(|s| s.goal.clone()).collect(),
    });

    let mut integrated_files: Vec<String> = Vec::new();
    // Unresolved review findings, per subtask, and how many met the gating bar
    // (spec 16). A subtask can be Done and still carry findings — "green, with
    // reservations" is a state the report has to be able to express.
    let mut findings: Vec<(String, Vec<sc_review::Finding>)> = Vec::new();
    let mut blocking_findings = 0usize;
    // Set when a review checkpoint stopped the run (spec 16 — "Gate"). Distinct
    // from quiescence: the board may still have ready work, which is exactly why
    // the report must not read as "everything that could be done was done".
    let mut stopped_at_checkpoint = false;

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
            let stop = integrate_with_retry(
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
                &mut findings,
                &mut blocking_findings,
                gate,
            );
            // A review checkpoint said stop (spec 16 — "Gate"). Everything already
            // integrated stays integrated; the remaining subtasks stay Pending and
            // the report says so. Stopping is not reverting, and it is not failing
            // — it is the honest "a human should look at this before we go on".
            if stop {
                stopped_at_checkpoint = true;
                break;
            }
        }
        if stopped_at_checkpoint {
            break;
        }
    }

    let (done, failed, pending) = board.tally();
    // Final integration verification (spec 08 step 5: "Only after integration
    // verification passes does the orchestrator finish"). The per-merge gate only
    // checks "didn't make it worse" — a worker's *partial* fix can keep the failing
    // count flat, integrate, and leave the board all-done over a still-red suite.
    // Re-run the whole suite once at the end so "done" means the mainline is
    // actually green, not merely that every subtask landed (honest stop, spec 06).
    // A run stopped at a review checkpoint is NOT all-done, even if every subtask
    // that ran happened to land: there is work the human has not yet let us do, and
    // reporting completion would be the dishonest flattening spec 06 forbids.
    let all_done = !stopped_at_checkpoint
        && board.all_done()
        && match &cfg.verify_command {
            Some(cmd) => {
                badness(&sc_verify::run_verification_in(
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
        findings,
        blocking_findings,
        stopped_at_checkpoint,
    }
}

enum Integration {
    /// The merge landed. Carries the changed paths and the **integrated diff** —
    /// what actually landed in mainline, not what the worker proposed. A proposal
    /// that was partially applied, or applied alongside another worker's, produces
    /// a different diff than the worker wrote, and the reviewed artifact must be
    /// the one that ships (spec 16).
    Accepted(Vec<String>, sc_review::IntegratedDiff),
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
///
/// Returns whether the **run** should stop here — true only when post-integration
/// review gated and the human checkpoint said so (spec 16). The subtask is always
/// completed and recorded first, so stopping never discards work that landed.
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
    findings: &mut Vec<(String, Vec<sc_review::Finding>)>,
    blocking_findings: &mut usize,
    gate: &dyn crate::review::ReviewGate,
) -> bool {
    let id = result.subtask_id.clone();
    let subtask = wave.iter().find(|s| s.id == id).cloned();

    // The whole-suite baseline for THIS subtask's free-text fallback: the failing
    // count just before this subtask's first merge (so the fallback can ask "did this
    // subtask's merge clear the suite?"). Only needed when frozen tests are unknown.
    let baseline = match (&cfg.verify_command, cfg.frozen_paths.is_empty()) {
        (Some(cmd), true) => Some(badness(&sc_verify::run_verification_in(
            &cfg.sandbox,
            workspace,
            cmd,
        ))),
        _ => None,
    };

    let mut attempt = 0usize;
    // Files accepted on the most recent attempt at which this subtask was fully
    // green. `Some` means the subtask has *already succeeded once*, which changes
    // what a later failure means (spec 16): a review-driven retry is speculative
    // improvement on verified-correct work, so if it comes back worse, the honest
    // outcome is to keep the green result and report the finding — not to fail a
    // subtask that demonstrably passed. Only review can produce such a retry;
    // test-driven retries never reach this state, because they only fire while the
    // subtask is still red.
    let mut green_files: Option<Vec<String>> = None;
    let mut review_findings: Vec<sc_review::Finding> = Vec::new();
    let mut review_blocking = 0usize;
    loop {
        sink.record(&SwarmEvent::WorkerFinished {
            subtask: id.clone(),
            summary: result.report_summary.clone(),
            proposal: result.proposal.clone(),
        });

        let outcome = integrate(orchestrator, workspace, &result, cfg);
        let (accepted_files, integrated_diff) = match &outcome {
            Integration::Accepted(files, diff) => (Some(files.clone()), Some(diff.clone())),
            Integration::Rejected(_) => (None, None),
        };

        // Decide the subtask's TRUE status from a scoped check, not the cumulative
        // gate alone (spec 08). A rejected merge is trivially incomplete.
        let residual: Vec<sc_verify::TestCase> = match &cfg.verify_command {
            // `max_subtask_retries == 0` restores today's behaviour (spec 08): no
            // scoped completion check, no retry — trust the cumulative gate alone.
            // Accept → Done (the final whole-suite verify is the only backstop).
            _ if cfg.max_subtask_retries == 0 && accepted_files.is_some() => Vec::new(),
            // No verify command: nothing to scope against. An accepted merge is taken
            // as complete; a rejected one is incomplete.
            None if accepted_files.is_some() => Vec::new(),
            None => vec![synthetic_failure("integration rejected")],
            // With a verify command, scope to THIS subtask's OWN test files, not the
            // whole frozen suite — a single-file subtask must be judged only by the test
            // for its file, or tests for not-yet-written files keep it red and revert it
            // (observed live 2026-06-14: every subtask saw "2 tests still red"). Map the
            // subtask's source files to their tests by basename; fall back to the whole
            // suite if no own-test is found (so a subtask is never un-gated).
            Some(cmd) => {
                let own = subtask
                    .as_ref()
                    .map(|s| own_tests(&s.files, &cfg.frozen_paths))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| cfg.frozen_paths.clone());
                scoped_failures(&cfg.sandbox, workspace, cmd, &own, baseline)
            }
        };

        if let (true, Some(files)) = (residual.is_empty(), accepted_files) {
            green_files = Some(files);

            // The suite is satisfied. That answers *does it work?*; review now asks
            // *should this code stay?* over the diff that actually landed (spec 16).
            // Runs here and nowhere else: a red subtask is rejected before review
            // is reached, because a reviewer's opinion about broken code is noise.
            let (review_outcome, found) = crate::review::review_integration(
                advisor,
                &id,
                subtask.as_ref(),
                integrated_diff.as_ref().unwrap_or(&Default::default()),
                workspace,
                &cfg.review,
                cfg.max_subtask_retries.saturating_sub(attempt),
                sink,
            );
            // A later attempt's findings replace an earlier one's: they describe the
            // diff that is actually in the workspace now.
            review_findings = found;
            // Counted from the findings themselves, not from the decision. When
            // review asks for a retry the decision is `Retry`, carrying no count —
            // but if that retry then goes nowhere and this attempt's result is the
            // one kept, these findings are exactly what a human needs to be told
            // about. Deriving the count from the findings keeps the two in step
            // whichever way the retry lands.
            review_blocking = review_findings
                .iter()
                .filter(|f| f.is_blocking(cfg.review.gate_at))
                .count();

            // A corroborated finding with retry budget left re-dispatches the
            // subtask with the deterministic evidence — exactly as still-failing
            // tests do. It shares the EXISTING budget: two independent retry
            // budgets multiply into a run that never terminates.
            if let crate::review::Outcome::Retry(feedback) = &review_outcome {
                if let Some(st) = &subtask {
                    attempt += 1;
                    sink.record(&SwarmEvent::SubtaskRetry {
                        subtask: id.clone(),
                        attempt,
                        max: cfg.max_subtask_retries,
                        failing_tests: Vec::new(),
                    });
                    let adv: Option<&dyn ModelBackend> = advisor.map(|a| a as &dyn ModelBackend);
                    let wb: &dyn ModelBackend = worker_backend;
                    result = crate::worker::run_worker_with_feedback(
                        wb,
                        adv,
                        st,
                        workspace,
                        &cfg.worker,
                        Some(feedback),
                    );
                    continue;
                }
            }

            // Done. When review gated, the subtask is STILL Done and the findings
            // ride along attached: the work is verified correct, and discarding
            // green integrated code over an unfixed finding is the worse outcome
            // (spec 16 — "Budgets, and the last retry"). Never dropped, never
            // silently accepted — reported, and counted for the checkpoint.
            //
            // Then, in `gate` mode, the checkpoint itself: a corroborated finding
            // at or above the gating severity stops the run for a human. Note the
            // order — the subtask is completed and its findings recorded BEFORE the
            // human is asked, so stopping never loses work that already landed.
            let stop = matches!(review_outcome, crate::review::Outcome::Gated { .. })
                && gate.checkpoint(&id, &review_findings, review_blocking)
                    == crate::review::Checkpoint::Stop;
            complete_subtask(
                &id,
                green_files.take().unwrap_or_default(),
                review_findings,
                review_blocking,
                board,
                integrated_files,
                findings,
                blocking_findings,
                sink,
            );
            return stop;
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
                        let predicament = sc_core::Predicament {
                            task: &st.goal,
                            plan: &format!("subtask {id}: {}", st.goal),
                            recent: &result.report_summary,
                            trigger: &format!("scoped tests still failing: {}", failing.join(", ")),
                        };
                        if let Some(advice) = sc_core::consult(adv, &predicament) {
                            sink.record(&SwarmEvent::AdvisorConsulted {
                                subtask: id.clone(),
                                advice: advice.clone(),
                            });
                            feedback.push('\n');
                            feedback.push_str(&sc_core::advice_observation(&advice));
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

        // Exhausted (or nothing to re-dispatch).
        //
        // If this subtask was ALREADY green on an earlier attempt, it is not
        // failed — it is kept. Only a review-driven retry can reach here in that
        // state, and that retry is speculative improvement on verified-correct
        // work: a worker asked to remove a duplicate may propose nothing usable, or
        // propose something the gate rejects. Failing the subtask for that would
        // throw away green, integrated code over an unfixed finding — exactly the
        // outcome spec 16 calls the worse one. So: Done, with the findings
        // attached, and the checkpoint left to say a human should look.
        if let Some(files) = green_files.take() {
            // Same checkpoint as the green path: an unresolved gating finding still
            // deserves a human, and asking after the subtask is recorded means a
            // stop never costs the work.
            let stop = review_blocking > 0
                && gate.checkpoint(&id, &review_findings, review_blocking)
                    == crate::review::Checkpoint::Stop;
            complete_subtask(
                &id,
                files,
                review_findings,
                review_blocking,
                board,
                integrated_files,
                findings,
                blocking_findings,
                sink,
            );
            return stop;
        }

        // Never green: mark Failed with the residual as the reason (spec 08 —
        // Failed, not Done; dependents block via quiescence).
        board.fail(&id);
        let reason = if failing.is_empty() {
            match &outcome {
                Integration::Rejected(r) => r.clone(),
                Integration::Accepted(..) => "subtask tests still failing".to_string(),
            }
        } else {
            format!("subtask tests still failing: {}", failing.join(", "))
        };
        sink.record(&SwarmEvent::Integrated {
            subtask: id.clone(),
            accepted: false,
            files: vec![reason],
        });
        // A failed subtask is spec 08's business, not review's — nothing here for a
        // review checkpoint to stop for.
        return false;
    }
}

/// Mark a subtask `Done` and record everything that rides along with it: the files
/// it landed, and any unresolved review findings (spec 16).
///
/// Findings do not change the status. "Green, with reservations" is a real state,
/// and the report has to be able to express it rather than flattening it to a pass
/// or a failure (spec 06 — honest stop).
#[allow(clippy::too_many_arguments)]
fn complete_subtask(
    id: &str,
    files: Vec<String>,
    review_findings: Vec<sc_review::Finding>,
    review_blocking: usize,
    board: &mut crate::board::TaskBoard,
    integrated_files: &mut Vec<String>,
    findings: &mut Vec<(String, Vec<sc_review::Finding>)>,
    blocking_findings: &mut usize,
    sink: &dyn SwarmSink,
) {
    board.complete(id);
    for f in &files {
        if !integrated_files.contains(f) {
            integrated_files.push(f.clone());
        }
    }
    *blocking_findings += review_blocking;
    if !review_findings.is_empty() {
        findings.push((id.to_string(), review_findings));
    }
    sink.record(&SwarmEvent::Integrated {
        subtask: id.to_string(),
        accepted: true,
        files,
    });
}

#[cfg(test)]
mod tests;
