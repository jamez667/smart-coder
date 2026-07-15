//! The agent loop: a bounded act → observe cycle (spec 03).
//!
//! One model turn = one tool call. The harness owns the budget and the
//! observation feedback; the model only ever decides the next single action.
//! Malformed output is a normal, handled condition — it's fed back through the
//! repair loop (spec 03), never acted on and never a crash.
//!
//! The loop is parameterized over a [`ToolRegistry`] and a [`ToolCallStrategy`]
//! (spec 04/02), so growing the tool surface or changing how tool calls are
//! decoded never touches this file.

use std::path::Path;
use std::sync::Arc;

use dc_context::{prompt_budget, truncate_observation, ContextBuilder, TokenCounter, TurnRecord};
use dc_index::Boosts;
use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;
use dc_tools::{Journal, PermissionPolicy, ToolOutcome, ToolRegistry};

use crate::confirm::Confirmer;
use crate::event::{AgentEvent, EventSink, NullSink};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::planner::make_plan;
use crate::recovery::{action_hash, StallDetector, StopReason};
use crate::strategy::ToolCallStrategy;
use crate::text::{first_line, mentioned_identifiers};

/// Loop configuration, including the Context Manager's budget knobs (spec 05).
///
/// `Debug` is hand-written (below) rather than derived because [`confirmer`] is a
/// trait object, which is not `Debug`.
///
/// [`confirmer`]: AgentConfig::confirmer
#[derive(Clone)]
pub struct AgentConfig {
    /// Hard cap on model turns (spec 03 — budgets are first-class).
    pub max_steps: usize,
    /// Fraction of the backend's advertised window we actually budget against —
    /// small models degrade before the nominal max (spec 05).
    pub effective_context_fraction: f64,
    /// Tokens reserved for the model's reply (subtracted from the budget).
    pub response_reserve_tokens: usize,
    /// Max lines kept from any single tool observation before truncation (spec 05). This
    /// is the cap for runaway command/test logs (a 5k-line pytest dump), where error-first
    /// truncation keeps the signal. File reads use the more generous `read_file_line_cap`.
    pub observation_line_cap: usize,
    /// Max lines kept from a `read_file` observation. A source file is not a runaway log —
    /// clipping it to `observation_line_cap` (40) amputates the very code the model must
    /// edit, so it re-reads or guesses. Give file reads real room to hold whole small/medium
    /// files; the general `observation_line_cap` still tames noisy command output.
    pub read_file_line_cap: usize,
    /// How many most-recent turns stay verbatim before older ones are compacted
    /// into a rolling summary (spec 05).
    pub keep_recent_turns: usize,
    /// How many top-ranked symbols the repo map injects into the retrieved zone.
    pub repo_map_top_k: usize,
    /// The permission gate consulted before every mutating/destructive call
    /// (spec 04). Defaults conservatively: edits auto, shell denied, frozen tests
    /// untouchable.
    pub permission: PermissionPolicy,
    /// The project's test command. When set, the loop runs verify-red-first and
    /// gates `finish` on a green whole suite (spec 11). `run_verification` uses it.
    pub verify_command: Option<String>,
    /// Ask the planner for a step plan before the loop (spec 03 — PLAN). When
    /// false, the agent runs plan-free (M0–M3 behavior).
    pub plan_first: bool,
    /// Consecutive identical actions before the harness intervenes (spec 03 — loop
    /// detection).
    pub repeat_limit: usize,
    /// Consecutive turns with no workspace change before intervening (stall).
    pub no_progress_limit: usize,
    /// Per-step retry budget: failed attempts on the active step before the
    /// harness gives up on it and moves on (spec 03).
    pub step_retry_budget: usize,
    /// An optional string appended to the system prompt — a model-quirk hook. Some
    /// small models need a directive to behave (e.g. Qwen3 needs `/no_think` or it
    /// burns its budget in a reasoning block and returns empty). Kept generic so
    /// the harness stays model-agnostic; the CLI sets it per model.
    pub system_suffix: Option<String>,
    /// Files the agent is scoped to edit. When set, the loop pins their *current*
    /// contents (re-read fresh every turn) into the retrieved zone, so a small model
    /// always has a correct, up-to-date view to anchor `edit_file` on without having
    /// to re-read — and, crucially, without the view ever going stale after an edit.
    /// Empty = no focus (the model navigates with read_file as usual). Set by the
    /// swarm, which scopes each worker to a disjoint set of files.
    pub focus_files: Vec<String>,
    /// Plan/preview only: when set, the loop runs read-only tools for real (so the
    /// model still sees true context) but **never** executes a side-effecting tool —
    /// edits, file creation, and shell/verification commands are short-circuited to
    /// a `[dry-run]` note instead of running (spec 06 `--dry-run`). The workspace is
    /// left untouched.
    pub dry_run: bool,
    /// Emit the fully-assembled prompt each turn as an [`AgentEvent::PromptAssembled`]
    /// — *what the model actually saw* (spec 06 `--verbose`, spec 05). Off by
    /// default because the payload is large; renderers/logs only get it when asked.
    pub verbose: bool,
    /// Optional human confirmer for confirm-gated shell commands (spec 04 / spec 06).
    /// When `None`, an unapproved `run_command` is auto-denied exactly as before
    /// (headless). When set, the loop blocks and asks before denying — the seam the
    /// GUI's approve/deny buttons and the CLI's interactive prompt drive. `Arc` keeps
    /// `AgentConfig: Clone` and lets the handle cross to the worker thread.
    pub confirmer: Option<Arc<dyn Confirmer>>,
    /// Where `run_verification` runs (spec 12): the host, or a per-run Docker container.
    /// Docker gives generated code a pinned toolkit + a known layout so the tests run
    /// against a reproducible env (the GUI defaults to it). Defaults to the host.
    pub sandbox: dc_verify::Sandbox,
    /// On a test-failure stall, run a root-cause diagnosis (a focused debugger pass over the
    /// FULL test output + all source files) and inject it, instead of the generic
    /// self-recovery directive (spec 03 — recovery). The model debugs blind otherwise: it
    /// reacts to a downstream symptom and edits the wrong file. Default OFF — it costs an
    /// extra suite run + model call per stall, so it ships dark and is enabled once proven
    /// on the ladder. Bounded by `DIAGNOSIS_LIMIT` and gated on a configured verify command.
    pub diagnose: bool,
    /// Stream each turn's generation, emitting [`AgentEvent::ContentDelta`] per token so a UI
    /// can show the reply — including a file edit being written — appear live, word by word.
    /// Off by default (the blocking `generate` path); the GUI's iterate/fix runs turn it on.
    pub stream: bool,
    /// Cooperative cancellation: when set and flipped to `true`, the loop stops at the next
    /// turn boundary with `StopReason::Cancelled` (it can't interrupt an in-flight model call,
    /// but won't start another). The GUI's Cancel button flips this. `Arc` keeps
    /// `AgentConfig: Clone` and lets the flag cross to the worker thread.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("max_steps", &self.max_steps)
            .field(
                "effective_context_fraction",
                &self.effective_context_fraction,
            )
            .field("response_reserve_tokens", &self.response_reserve_tokens)
            .field("observation_line_cap", &self.observation_line_cap)
            .field("read_file_line_cap", &self.read_file_line_cap)
            .field("keep_recent_turns", &self.keep_recent_turns)
            .field("repo_map_top_k", &self.repo_map_top_k)
            .field("permission", &self.permission)
            .field("verify_command", &self.verify_command)
            .field("plan_first", &self.plan_first)
            .field("repeat_limit", &self.repeat_limit)
            .field("no_progress_limit", &self.no_progress_limit)
            .field("step_retry_budget", &self.step_retry_budget)
            .field("system_suffix", &self.system_suffix)
            .field("focus_files", &self.focus_files)
            .field("dry_run", &self.dry_run)
            .field("verbose", &self.verbose)
            // `dyn Confirmer` isn't `Debug`; report presence only.
            .field("confirmer", &self.confirmer.is_some())
            .field("sandbox", &self.sandbox)
            .field("diagnose", &self.diagnose)
            .finish()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            effective_context_fraction: 0.75,
            response_reserve_tokens: 1024,
            observation_line_cap: 40,
            read_file_line_cap: 400,
            keep_recent_turns: 3,
            repo_map_top_k: 30,
            permission: PermissionPolicy::default(),
            verify_command: None,
            plan_first: false,
            repeat_limit: 3,
            no_progress_limit: 4,
            step_retry_budget: 3,
            system_suffix: None,
            focus_files: Vec::new(),
            dry_run: false,
            verbose: false,
            confirmer: None,
            sandbox: dc_verify::Sandbox::default(),
            diagnose: false,
            stream: false,
            cancel: None,
        }
    }
}

/// What happened over a run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReport {
    /// Whether the model called `finish` within budget.
    pub finished: bool,
    /// Model turns taken.
    pub steps: usize,
    /// Tool-call validity metrics over the run (spec 07 — the M1 ≥95% target).
    pub metrics: ToolCallMetrics,
    /// The largest assembled-prompt token count over the run, and the hard budget
    /// it was kept under (spec 05 — the window is a hard-budgeted resource).
    pub peak_prompt_tokens: usize,
    pub prompt_budget: usize,
    /// Whether the configured verification command was green at `finish` (spec 11
    /// — the whole-suite gate). `None` if no `verify_command` was configured.
    pub verified: Option<bool>,
    /// A compact summary of files changed over the run (spec 04/06 — the journal's
    /// diff overview).
    pub change_summary: String,
    /// Why the run stopped (spec 06 — honest stop lines). `finished` is a
    /// convenience alias for `stop_reason == Finished`.
    pub stop_reason: StopReason,
    /// How many times the harness intervened (re-plan / advisor nudge) to recover
    /// the agent from a stall (spec 03).
    pub interventions: usize,
}

const TASK_PREFIX: &str = "You are a coding agent working in a project directory. \
Make the failing test pass. Follow this loop: \
1) read_file the file you need to change (don't just search repeatedly); \
2) edit_file it with a precise change; \
3) run_verification to run the tests (use run_verification, NOT run_command — \
shell is blocked); read which tests still fail and fix them; \
4) finish only when the tests pass. \
Take a concrete action every turn — prefer editing over searching.\n\n";

/// System preamble for a focus-scoped run: the file you must edit is already shown
/// to you every turn, so don't read it — edit it. Used by the swarm worker (and
/// any caller that sets `focus_files`).
const FOCUS_TASK_PREFIX: &str = "You fix code. The file you must change is shown below IN FULL, \
between === markers — it updates after each edit, so never read it again. The files it imports \
from are also shown in full as READ-ONLY context (between --- markers); any remaining files \
appear as a signature map (`path:line  name`). You already have everything you need — do NOT \
read_file any of these. Each turn, do ONE of:\n\
- edit_file / write_file: change the shown file. Copy old_str exactly from it.\n\
- run_verification: run the tests to see what still fails.\n\
- finish: stop, once the tests pass.\n\
Edit the shown file (using the imported files and the map for context), verify, repeat.\n\n";

/// Run the agent against `instruction` in `workspace` with the default registry,
/// choosing the strongest tool-call strategy the backend can enforce (spec 02).
pub fn run_agent(
    backend: &dyn ModelBackend,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    let registry = dc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    run_agent_with(
        backend,
        &registry,
        strategy.as_ref(),
        instruction,
        workspace,
        cfg,
    )
}

/// Run the agent with an explicit registry and tool-call strategy, no planner or
/// advisor (the M0–M3 behavior). For planning + recovery, use
/// [`run_agent_recovering`].
pub fn run_agent_with(
    backend: &dyn ModelBackend,
    registry: &ToolRegistry,
    strategy: &dyn ToolCallStrategy,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    run_agent_recovering(
        backend,
        None,
        registry,
        strategy,
        instruction,
        workspace,
        cfg,
    )
}

/// Run the agent with planning and recovery (spec 03 — M4).
///
/// * `backend` is the coder (T2). If `cfg.plan_first`, it is also asked to plan.
/// * `advisor` is the optional senior model (T1) consulted when the agent stalls
///   — "junior asks senior" (spec 02). It gives a *hint*, not the fix.
///
/// The harness owns the plan, detects loops/stalls, and decides when to re-plan,
/// nudge via the advisor, or stop — the model never has to.
pub fn run_agent_recovering(
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    registry: &ToolRegistry,
    strategy: &dyn ToolCallStrategy,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    run_agent_observed(
        backend,
        advisor,
        registry,
        strategy,
        instruction,
        workspace,
        cfg,
        &NullSink,
    )
}

/// Like [`run_agent_recovering`] but streams typed [`AgentEvent`]s to `sink` as
/// the run unfolds (spec 01) — the seam a live TUI, `--json`, or a session log
/// consumes. The behavior is identical; only observation is added.
#[allow(clippy::too_many_arguments)]
pub fn run_agent_observed(
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    registry: &ToolRegistry,
    strategy: &dyn ToolCallStrategy,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
    sink: &dyn EventSink,
) -> Result<AgentReport> {
    // Centralized run log: tee every event into a queryable in-process store (spec 01). The
    // loop can then read earlier results (e.g. the last verification output for the diagnostic)
    // with code, instead of re-running to recover what it already saw. `sink` is shadowed so
    // all existing `sink.record(...)` calls fan out to both the caller's sink and the log.
    let runlog = crate::runlog::RunLogSink::new();
    let tee = crate::event::TeeSink::new(vec![sink, &runlog]);
    let sink: &dyn EventSink = &tee;

    // When the agent is scoped to focus files, the loop pins their live contents
    // every turn — so the system prompt must NOT tell the model to read first
    // (that just traps a tiny model in a read loop). Lead with "edit" instead.
    let prefix = if cfg.focus_files.is_empty() {
        TASK_PREFIX
    } else {
        FOCUS_TASK_PREFIX
    };
    let mut system = format!("{prefix}{}", strategy.system_preamble(registry));
    if let Some(suffix) = &cfg.system_suffix {
        system.push('\n');
        system.push_str(suffix);
    }

    // Token accounting + hard budget (spec 05).
    let counter = TokenCounter::new(backend);
    let caps = backend.capabilities();
    let budget = prompt_budget(
        caps.max_context_tokens,
        cfg.effective_context_fraction,
        cfg.response_reserve_tokens,
    );
    let builder = ContextBuilder::new(&counter, budget);

    // The repo map is stable retrieval; boost task-named symbols (spec 05, aider).
    let repo_map = dc_index::repo_map(
        workspace,
        &Boosts {
            mentioned_symbols: mentioned_identifiers(instruction),
            in_play_files: Vec::new(),
        },
        cfg.repo_map_top_k,
    );

    sink.record(&AgentEvent::RunStarted {
        task: instruction.to_string(),
        prompt_budget: budget,
    });

    // PLAN (spec 03): decompose the task up front, grounded in the repo map. The
    // harness owns the plan; the model only ever sees a compact rendering.
    let mut plan = if cfg.plan_first {
        make_plan(backend, instruction, &repo_map)?
    } else {
        PlanState::default()
    };
    if !plan.is_empty() {
        sink.record(&AgentEvent::Planned {
            steps: plan.steps().iter().map(|s| s.description.clone()).collect(),
        });
    }

    let mut metrics = ToolCallMetrics::default();
    let mut history: Vec<TurnRecord> = Vec::new();
    let mut recent: Vec<Message> = Vec::new();
    let mut peak_prompt_tokens = 0usize;
    let mut journal = Journal::new();
    let mut stall_detector = StallDetector::default();
    // The harness's in-loop intervention bookkeeping: the running intervention count, the
    // bounded diagnosis/self-recovery counters the stall ladder spends, and the previous-action
    // hash the repeat-dedup guard clears on recovery (spec 02/03). See [`stall::Interventions`].
    let mut interv = stall::Interventions::default();
    // How many turns in a row we've had to nudge the model off an idempotent
    // repeat. If a nudge doesn't land, escalate to the advisor rather than nudging
    // forever (spec 02 — junior asks senior).
    let mut nudge_streak = 0usize;
    // A failing `edit_file` on this path, and how many times in a row. A small model
    // often anchors `edit_file` on code it *imagines* it wrote (e.g. a `jsonify(...)`
    // line that isn't in the file), so the anchor never matches and it loops. After a
    // couple of misses the harness tells it to stop fiddling with anchors and rewrite
    // the whole (small) file with `create_file` — far more reliable than a perfect
    // anchor. Observed live 2026-06-15 (the A/B `/sum` 500→400 fix it couldn't apply).
    let mut failed_edit_path: Option<String> = None;
    let mut failed_edit_streak = 0usize;
    // The same verification failure, seen N times in a row. A model stuck on a hard bug
    // edits ineffectively (each edit resets the stall, so the stall detector never trips)
    // or spams run_verification — burning the whole budget while the SAME tests keep
    // failing. When the failure signature is unchanged across several verifications, the
    // harness escalates: quote the exact failing tests and demand a full rewrite of the
    // offending file (observed live 2026-06-15: the ladder's expr-eval/root-cause rungs
    // looped ~10 verifications on an unchanged failure and died at the step budget).
    // Shell-command approvals accumulated this run via `Confirmation::AllowRemember`
    // (spec 06). Owned by the loop and mutated in place, so `cfg` stays shared and
    // `PermissionPolicy` is never mutated. Checked in addition to the static policy.
    let mut session_allow: Vec<String> = Vec::new();

    for step in 0..cfg.max_steps {
        // Cooperative cancel: if the user hit Cancel, stop cleanly at this turn boundary
        // (we can't interrupt an in-flight model call, but we won't start another).
        if cfg
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        {
            sink.record(&AgentEvent::Stopped {
                reason: StopReason::Cancelled,
            });
            return Ok(AgentReport {
                finished: false,
                steps: step,
                metrics,
                peak_prompt_tokens,
                prompt_budget: budget,
                verified: None,
                change_summary: journal.change_summary(),
                stop_reason: StopReason::Cancelled,
                interventions: interv.count,
            });
        }
        // Assemble the budgeted, zoned prompt (spec 05): compact older turns, zone the plan +
        // retrieval + sacred recent window, and note which files are pinned in full this turn.
        let (segments, pinned_full_files) = assemble::assemble_segments(
            cfg,
            workspace,
            instruction,
            &system,
            &repo_map,
            plan.render(),
            &history,
            &recent,
        );

        let built = builder.build(segments);
        peak_prompt_tokens = peak_prompt_tokens.max(built.tokens_used);

        // Verbose (spec 06): surface the exact assembled prompt before it's sent, so
        // a renderer/log can show what the model actually saw. Gated — the payload
        // is large, so normal runs never carry it.
        if cfg.verbose {
            sink.record(&AgentEvent::PromptAssembled {
                step: step + 1,
                tokens: built.tokens_used,
                messages: built
                    .messages
                    .iter()
                    .map(|m| crate::event::PromptMessage {
                        role: role_word(m.role).to_string(),
                        content: m.content.clone(),
                    })
                    .collect(),
            });
        }

        let mut req = GenerateRequest::new(built.messages);
        strategy.prepare_request(&mut req, registry);
        // Stream the turn when enabled, emitting a ContentDelta per token so a UI can show the
        // reply (incl. a file edit being written) appear live. Falls back to blocking generate
        // when off. Streaming is pure observation — the decode/apply path below is unchanged.
        let resp = if cfg.stream {
            let step_num = step + 1;
            let mut cumulative = String::new();
            let mut on_token = |delta: &str| {
                cumulative.push_str(delta);
                sink.record(&AgentEvent::ContentDelta {
                    step: step_num,
                    cumulative: cumulative.clone(),
                });
            };
            backend.generate_streaming(&req, &mut on_token)?
        } else {
            backend.generate(&req)?
        };
        // Emit the model's full raw output for this turn (spec 06 — show what the
        // model actually said).
        sink.record(&AgentEvent::ModelTurn {
            step: step + 1,
            prompt_tokens: built.tokens_used,
            raw: resp.content.clone(),
        });

        // Decode the tool call.
        // Decode the tool call. If extraction fails but the model replied with a fenced code
        // block AND the step is scoped to a single file (a per-file step), recover a
        // `write_file` of that block to the focused file — the model "thought out loud" and
        // wrote the file as ```python```, its natural format, instead of a JSON tool call
        // (observed: a per-file step burned its whole budget being rejected for this). This
        // turns a wasted turn into the write the model intended.
        let extracted = strategy.extract(&resp.content, registry).or_else(|e| {
            if cfg.focus_files.len() == 1 {
                crate::strategy::extract_markdown_write(
                    &resp.content,
                    &cfg.focus_files[0],
                    registry,
                )
                .ok_or(e)
            } else {
                Err(e)
            }
        });
        // Did this turn's call come from truncation salvage (a write_file whose content was cut
        // off mid-string)? If so, the file now holds only the partial head, and re-writing the
        // whole thing next turn would just truncate at the same place — so we steer the model to
        // append_file the remainder instead. Detected by the recovery firing on THIS raw reply.
        let salvaged_truncated_write = extracted.as_ref().is_ok_and(|c| {
            matches!(c.name.as_str(), "write_file" | "append_file")
                && crate::strategy::is_truncated_write_salvage(&resp.content, registry)
        });
        let (obs, action, changed, tool, arg) = match extracted {
            Ok(call) => {
                metrics.record_valid();
                let arg = key_arg(&call);
                let action = action_hash(&call.name, &arg);
                let tool = call.name.clone();
                sink.record(&AgentEvent::ToolCall {
                    tool: tool.clone(),
                    arg: arg.clone(),
                });

                // Meta-tools the harness owns (spec 03/04) — never hit fs/exec.
                if call.name == "update_plan" {
                    let steps = crate::planner::parse_plan(call.str("steps").unwrap_or_default());
                    let obs = if steps.is_empty() {
                        "update_plan: could not parse a step array; plan unchanged".to_string()
                    } else {
                        plan = PlanState::from_descriptions(steps);
                        sink.record(&AgentEvent::PlanRevised {
                            steps: plan.steps().iter().map(|s| s.description.clone()).collect(),
                        });
                        format!("update_plan: ok\n{}", plan.render())
                    };
                    (obs, action, false, tool, arg)
                } else if call.name == "ask_user" {
                    // Junior asks senior (spec 02). Consult the advisor for a nudge.
                    let question = call.str("question").unwrap_or_default();
                    match escalate(advisor, instruction, &plan, &history, question) {
                        Some(advice) => {
                            interv.count += 1;
                            stall_detector.reset();
                            sink.record(&AgentEvent::Advice {
                                trigger: format!("ask_user: {question}"),
                                advice: advice.clone(),
                            });
                            (advice, action, false, tool, arg)
                        }
                        None => {
                            let reason = StopReason::Escalated(question.to_string());
                            sink.record(&AgentEvent::Stopped {
                                reason: reason.clone(),
                            });
                            return Ok(stopped(
                                reason,
                                step + 1,
                                &cfg.sandbox,
                                &cfg.verify_command,
                                workspace,
                                &journal,
                                metrics,
                                peak_prompt_tokens,
                                budget,
                                interv.count,
                            ));
                        }
                    }
                } else if call.name == "read_file"
                    && pinned_full_files
                        .iter()
                        .any(|f| Some(f.as_str()) == call.str("path"))
                {
                    // Short-circuit a read of a file whose CURRENT contents are already pinned
                    // in this turn's prompt (the focus file or an imported one). The model
                    // re-reads pinned files reflexively — even its own focus file — and the
                    // immediate-repeat guard misses interleaved re-reads (read a, read b, read
                    // a). Redirect it to the shown copy instead of spending a turn on the read.
                    let path = call.str("path").unwrap_or_default().to_string();
                    let obs = format!(
                        "`{path}` is ALREADY SHOWN IN FULL above (between the file markers) and \
                         updates after each edit — you do not need to read it. Edit it directly \
                         (or, if it's read-only context, just use it). Make your next change now."
                    );
                    (obs, action, false, tool, arg)
                } else {
                    // Batched whole-file writes (spec 03 / thread 3): a capable model emits
                    // the entire solution as many tool calls in ONE turn. The loop runs one
                    // action per turn, so the leading run of distinct-path create/write calls
                    // beyond the first used to be discarded — the model then re-emitted them
                    // turn after turn (a long grind / stall). Creating several DIFFERENT files
                    // is order-independent and needs no observe→react between them, so when the
                    // first call IS such a write, pre-apply the rest of the safe leading batch
                    // here (strictly gated by extract_write_batch). The first call still flows
                    // through the normal dispatch below; this only adds the extra writes.
                    let batch_note = if matches!(call.name.as_str(), "write_file" | "create_file")
                        && !cfg.dry_run
                    {
                        pre_apply_batched_writes(
                            &resp.content,
                            registry,
                            &cfg.permission,
                            workspace,
                            &mut journal,
                            sink,
                        )
                    } else {
                        String::new()
                    };

                    // A normal tool call. Snapshot for the journal, then dispatch.
                    let pre = mutating_path(&call, registry)
                        .map(|p| (p.clone(), Journal::snapshot(workspace, &p)));
                    let outcome = dispatch(
                        &call,
                        registry,
                        &cfg.permission,
                        cfg.confirmer.as_deref(),
                        &mut session_allow,
                        &cfg.sandbox,
                        &cfg.verify_command,
                        cfg.dry_run,
                        workspace,
                    );
                    let changed = pre
                        .map(|(path, before)| {
                            let after = Journal::snapshot(workspace, &path);
                            let did_change = before != after;
                            journal.record(workspace, &path, before);
                            did_change
                        })
                        .unwrap_or(false);

                    match outcome {
                        ToolOutcome::Finished => {
                            match gate_finish(&cfg.sandbox, &cfg.verify_command, workspace) {
                                FinishGate::Allow(verified) => {
                                    if let Some(v) = verified {
                                        sink.record(&AgentEvent::Verification {
                                            green: v,
                                            summary: "whole-suite gate passed".to_string(),
                                            full: "whole-suite gate passed".to_string(),
                                        });
                                    }
                                    sink.record(&AgentEvent::Stopped {
                                        reason: StopReason::Finished,
                                    });
                                    return Ok(AgentReport {
                                        finished: true,
                                        steps: step + 1,
                                        metrics,
                                        peak_prompt_tokens,
                                        prompt_budget: budget,
                                        verified,
                                        change_summary: journal.change_summary(),
                                        stop_reason: StopReason::Finished,
                                        interventions: interv.count,
                                    });
                                }
                                FinishGate::Refuse(o) => {
                                    sink.record(&AgentEvent::Verification {
                                        green: false,
                                        summary: "finish refused — suite still red".to_string(),
                                        full: o.clone(),
                                    });
                                    // Tests red — a failed attempt on the active step.
                                    if plan.record_attempt() > cfg.step_retry_budget {
                                        plan.fail_active();
                                    }
                                    (o, action, false, tool, arg)
                                }
                            }
                        }
                        ToolOutcome::Observation(o) => {
                            if tool == "run_verification" {
                                // Only a *configured* verification with real test
                                // detail counts as green (the "no command" message
                                // isn't a pass).
                                let configured = cfg.verify_command.is_some();
                                let green = configured && !looks_like_failure(&o);
                                sink.record(&AgentEvent::Verification {
                                    green,
                                    summary: first_line(&o),
                                    full: o.clone(),
                                });
                                // Auto-finish: if the suite is green, the task is
                                // done — a small model that forgets to call `finish`
                                // shouldn't lose a win it already earned (spec 11).
                                if green {
                                    sink.record(&AgentEvent::Stopped {
                                        reason: StopReason::Finished,
                                    });
                                    return Ok(AgentReport {
                                        finished: true,
                                        steps: step + 1,
                                        metrics,
                                        peak_prompt_tokens,
                                        prompt_budget: budget,
                                        verified: Some(true),
                                        change_summary: journal.change_summary(),
                                        stop_reason: StopReason::Finished,
                                        interventions: interv.count,
                                    });
                                }
                            }
                            // Prepend the note about any extra files the batch pre-applied,
                            // so the model's next observation reflects ALL the writes (not just
                            // the first), and a change anywhere in the batch counts as progress.
                            let o = if batch_note.is_empty() {
                                o
                            } else {
                                format!("{batch_note}{o}")
                            };
                            // If this write was salvaged from a truncated reply, only the partial
                            // head landed. Tell the model to CONTINUE with append_file rather than
                            // re-writing the whole file (which would truncate at the same place).
                            let o = if salvaged_truncated_write {
                                format!(
                                    "{o}\nNOTE: your reply was cut off, so only the part above was \
                                     saved. Do NOT re-send the whole file — continue it with \
                                     append_file (same path), adding the NEXT chunk only. Repeat \
                                     append_file until the file is complete."
                                )
                            } else {
                                o
                            };
                            let changed = changed || !batch_note.is_empty();
                            (o, action, changed, tool, arg)
                        }
                    }
                }
            }
            // Repair loop (spec 03): feed back the exact error; never execute.
            Err(e) => {
                metrics.record_invalid();
                let detail = e.repair_prompt();
                sink.record(&AgentEvent::RepairTriggered {
                    detail: first_line(&detail),
                });
                (
                    detail,
                    action_hash("(malformed)", ""),
                    false,
                    "(malformed)".to_string(),
                    String::new(),
                )
            }
        };

        // Repeat-dedup (spec 03): a tiny model often re-issues the *same*
        // idempotent call (`read_file mathlib.py`, or `run_verification` over and
        // over) instead of acting on what it already has — burning the budget until
        // the stall trips. When the action exactly repeats such a tool with nothing
        // changed between, replace the (identical) observation with a terse nudge
        // toward the actual edit. This breaks the loop a turn earlier than the stall
        // detector and points the model at the next concrete move.
        let (obs, action, changed, tool, arg) =
            if interv.prev_action == Some(action) && is_idempotent_tool(&tool) {
                nudge_streak += 1;
                // If a nudge already failed to move the model, stop nudging and ask
                // the senior for a concrete hint (spec 02). The advisor sees the
                // recent history and the workspace state via the predicament.
                let escalated = if nudge_streak >= 2 {
                    escalate(
                        advisor,
                        instruction,
                        &plan,
                        &history,
                        &format!("model keeps repeating `{tool}` without making the fix"),
                    )
                } else {
                    None
                };
                let obs = match escalated {
                    Some(advice) => {
                        interv.count += 1;
                        nudge_streak = 0;
                        sink.record(&AgentEvent::Advice {
                            trigger: format!("repeating {tool}"),
                            advice: advice.clone(),
                        });
                        advice
                    }
                    None if tool == "run_verification" => {
                        "You just ran the tests and nothing has changed since — re-running \
                         gives the same result. The suite is still failing: change the code \
                         to fix the reported failure (use `write_file` to write/overwrite a \
                         whole file, or `edit_file` for a small anchored change), then \
                         run_verification."
                            .to_string()
                    }
                    None => format!(
                        "You already have the result of `{tool}` — re-running it changes \
                         nothing. Take a CONCRETE next action now: if a source file the tests \
                         need does not exist yet, create it with `write_file` (the ENTIRE file \
                         contents in one shot); if it exists but a test is failing, fix it with \
                         `write_file` (whole file) or `edit_file` (anchored change), then \
                         run_verification."
                    ),
                };
                // Fix #2: the PRIOR turn's successful result of this same idempotent call is
                // still the last user message in `recent` — the model trusts that concrete
                // "it worked" output over the nudge sitting next to it. Supersede it so the
                // nudge isn't drowned by a visible success of the very call we're discouraging.
                replace_last_user(
                    &mut recent,
                    &format!("[earlier `{tool}` result superseded — act on the note below]"),
                );
                (obs, action, false, tool, arg)
            } else {
                nudge_streak = 0;
                (obs, action, changed, tool, arg)
            };
        interv.prev_action = Some(action);

        // edit_file anchor-loop breaker (spec 03): a non-matching `edit_file` (the
        // anchor isn't in the file) is a mutating call that errored, so the
        // idempotent-repeat path above never catches it — yet a small model will
        // re-submit the same imagined anchor until the stall kills it. Track repeated
        // misses on the same path and, after a couple, steer it to rewrite the whole
        // file with `create_file` instead of hunting for an anchor that doesn't exist.
        // Two failure modes, one cure (`write_file`):
        //  - `edit_file` whose anchor isn't in the file (model imagines the contents).
        //  - `create_file` on a path that already exists (create_file refuses to
        //    overwrite, so the model that wants to FIX a file it already wrote loops on
        //    `create_file` forever — observed live 2026-06-15, the multi-file db task
        //    died this way after writing app.py once). Both mean "rewrite this file".
        let edit_missed = tool == "edit_file"
            && (obs.contains("0 matches") || obs.contains("not found"))
            && !changed;
        let create_clash = tool == "create_file" && obs.contains("already exists") && !changed;
        // write_file REJECTED because the target is too large to safely overwrite — the model
        // fixates on write_file and re-submits it every turn, ignoring the "use edit_file/
        // append_file" steer in the rejection (observed live 2026-07-15: ~10 write_file
        // rejections in a row on a stage). Track it like an edit-miss so the breaker fires a
        // firm directive and resets, instead of the stall detector slowly killing the stage.
        let write_blocked =
            tool == "write_file" && obs.contains("too large to safely overwrite") && !changed;
        let write_loop = edit_missed || create_clash || write_blocked;
        if write_loop && failed_edit_path.as_deref() == Some(arg.as_str()) {
            failed_edit_streak += 1;
        } else if write_loop {
            failed_edit_path = Some(arg.clone());
            failed_edit_streak = 1;
        } else {
            failed_edit_path = None;
            failed_edit_streak = 0;
        }
        let obs = if failed_edit_streak >= 2 {
            failed_edit_path = None;
            failed_edit_streak = 0;
            interv.count += 1;
            // Is the target a LARGE existing file? A wholesale `write_file` of such a file
            // corrupts it (the model can't reproduce hundreds of lines faithfully — unterminated
            // strings, dropped fns) AND is refused by the write_file guard, so steering to it
            // would deadlock. For a big file, steer to SURGICAL edits instead.
            let big_existing = std::fs::read_to_string(workspace.join(&arg))
                .map(|s| s.lines().count() > 150)
                .unwrap_or(false);
            let directive = if create_clash && !big_existing {
                format!(
                    "`{arg}` already exists — `create_file` will NOT overwrite it, so \
                     repeating it does nothing. To change it, call `write_file` with `path` \
                     `{arg}` and the ENTIRE new file contents in one shot (write_file \
                     overwrites). Make the fix the failing test needs."
                )
            } else if big_existing {
                format!(
                    "Your `edit_file` anchor is NOT in `{arg}` — you are matching against code \
                     that isn't there (often lines you INTEND TO ADD, written into old_str as if \
                     already present). `{arg}` is a large file: do NOT try to rewrite it whole. \
                     Instead: (1) to ADD new code (a struct field, a method, a match arm), copy a \
                     SHORT exact anchor — one or two REAL lines from the CURRENT file shown in the \
                     error above — and put ONLY those real lines in old_str, with the addition in \
                     new_str; or (2) to add a whole new method/function, use `append_file` to put \
                     it at the end of the file. Never include not-yet-existing lines in old_str."
                )
            } else {
                format!(
                    "Your `edit_file` anchor does not exist in `{arg}` — you are matching \
                     against code that isn't in the file. STOP editing by anchor. Instead call \
                     `write_file` with `path` `{arg}` and the ENTIRE corrected file contents in \
                     one shot (write_file overwrites the existing file). Base it on the file \
                     shown in the error above plus the fix the failing test needs."
                )
            };
            sink.record(&AgentEvent::Advice {
                trigger: if create_clash {
                    "create_file keeps clashing with an existing file".to_string()
                } else {
                    "edit_file anchor keeps missing".to_string()
                },
                advice: directive.clone(),
            });
            directive
        } else {
            obs
        };

        // Record the turn and detect stalls (spec 03 — VERIFY, cheap every turn).
        let was_error = looks_like_failure(&obs);
        sink.record(&AgentEvent::ToolResult {
            summary: first_line(&obs),
            full: obs.clone(),
            is_error: was_error,
        });
        history.push(TurnRecord::new(tool.clone(), arg, was_error));
        let trimmed = truncate_observation(&obs, observation_cap_for(&tool, cfg), true);
        push_recent(&mut recent, &resp.content, &trimmed, cfg.keep_recent_turns);

        // Auto test-repair (spec 03): the moment an edit lands, the harness runs
        // the suite itself — the model shouldn't have to remember to verify. If
        // it's green the task is done (auto-finish); if not, the failures re-enter
        // the loop as a fresh observation the model reacts to.
        if changed {
            if let Some(cmd) = &cfg.verify_command {
                // Run once, keep BOTH the raw output (the lossless record the run log stores,
                // for the diagnostic) and the parsed report (the failure-first form the model
                // reacts to). Before, only the compact observation was kept and the raw dump
                // was lost — so the diagnostic had to re-run the suite to recover it.
                let cmd_result = dc_verify::run_command_in(&cfg.sandbox, workspace, cmd);
                let report = dc_verify::parse(cmd, &cmd_result.output, cmd_result.ok);
                sink.record(&AgentEvent::Verification {
                    green: report.all_green(),
                    summary: first_line(&report.observation()),
                    full: cmd_result.output.clone(),
                });
                if report.all_green() {
                    sink.record(&AgentEvent::Stopped {
                        reason: StopReason::Finished,
                    });
                    return Ok(AgentReport {
                        finished: true,
                        steps: step + 1,
                        metrics,
                        peak_prompt_tokens,
                        prompt_budget: budget,
                        verified: Some(true),
                        change_summary: journal.change_summary(),
                        stop_reason: StopReason::Finished,
                        interventions: interv.count,
                    });
                } else {
                    // Surface the failing tests so the next turn is grounded.
                    let fb = format!(
                        "(harness ran the tests after your edit)\n{}",
                        report.observation()
                    );
                    push_observation(
                        &mut recent,
                        // Use the generous read_file cap, not the tight log cap: the report
                        // is failure-first and carries the underlying exception (e.g.
                        // TemplateNotFound) that the model must see to fix the bug. At the
                        // 40-line log cap the `✗`/assert headers crowded the real exception
                        // out, so the model only saw a bare `assert ... == ...` (observed
                        // live) and looped blind. 400 lines still bounds a degenerate suite.
                        &truncate_observation(&fb, cfg.read_file_line_cap, true),
                        cfg.keep_recent_turns,
                    );
                    // A failed auto-verify resets the stall streak: real progress
                    // was attempted, so don't count the edit+verify as "stuck".
                    stall_detector.reset();
                }
            }
        }

        match stall::handle_stall(
            action,
            changed,
            &mut interv,
            &mut stall_detector,
            &mut recent,
            &history,
            &plan,
            cfg,
            backend,
            advisor,
            instruction,
            workspace,
            &runlog,
            sink,
        ) {
            stall::StallDecision::Continue | stall::StallDecision::Recovered => {}
            stall::StallDecision::GiveUp(reason) => {
                sink.record(&AgentEvent::Stopped {
                    reason: reason.clone(),
                });
                return Ok(stopped(
                    reason,
                    step + 1,
                    &cfg.sandbox,
                    &cfg.verify_command,
                    workspace,
                    &journal,
                    metrics,
                    peak_prompt_tokens,
                    budget,
                    interv.count,
                ));
            }
        }
    }

    sink.record(&AgentEvent::Stopped {
        reason: StopReason::BudgetExhausted,
    });
    Ok(stopped(
        StopReason::BudgetExhausted,
        cfg.max_steps,
        &cfg.sandbox,
        &cfg.verify_command,
        workspace,
        &journal,
        metrics,
        peak_prompt_tokens,
        budget,
        interv.count,
    ))
}

mod assemble;
mod dispatch;
mod escalation;
mod prompt;
mod stall;
mod window;

#[cfg(test)]
mod test_util;
#[cfg(test)]
mod tests;

use dispatch::{
    dispatch, gate_finish, is_idempotent_tool, key_arg, looks_like_failure, mutating_path,
    observation_cap_for, pre_apply_batched_writes, FinishGate,
};
use escalation::{escalate, stopped};
use window::{push_observation, push_recent, replace_last_user, role_word};
