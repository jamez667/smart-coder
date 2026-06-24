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

use dc_context::{
    prompt_budget, summarize_history, truncate_observation, ContextBuilder, Segment, TokenCounter,
    TurnRecord, Zone,
};
use dc_index::Boosts;
use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;
use dc_tools::{execute, Journal, PermissionPolicy, ToolOutcome, ToolRegistry};

use crate::advisor::{advice_observation, consult, Predicament};
use crate::confirm::{Confirmation, Confirmer};
use crate::event::{AgentEvent, EventSink, NullSink};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::planner::make_plan;
use crate::recovery::{action_hash, Progress, StallDetector, StopReason};
use crate::strategy::ToolCallStrategy;

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
    /// Max lines kept from any single tool observation before truncation (spec 05).
    pub observation_line_cap: usize,
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
const FOCUS_TASK_PREFIX: &str = "You fix code. The file you must change is shown \
below, between === markers. Each turn, do ONE of:\n\
- edit_file: change the code. Copy old_str exactly from the file shown below.\n\
- run_verification: run the tests to see what still fails.\n\
- finish: stop, once the tests pass.\n\
Edit, then verify, then edit again until the tests pass.\n\n";

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
    let mut stall = StallDetector::default();
    let mut interventions = 0usize;
    // The previous turn's action hash, used to short-circuit a tiny model that
    // re-reads a file it already has instead of editing (see repeat-dedup below).
    let mut prev_action: Option<u64> = None;
    // How many turns in a row we've had to nudge the model off an idempotent
    // repeat. If a nudge doesn't land, escalate to the advisor rather than nudging
    // forever (spec 02 — junior asks senior).
    let mut nudge_streak = 0usize;
    // How many times we've self-recovered from a stall WITHOUT an advisor. A single
    // capable model has no senior to ask, so the harness steers it back in-band with
    // a firm directive instead of dying on the first loop. Bounded so a genuinely
    // stuck model still terminates (spec 03 — the harness owns recovery).
    let mut self_recoveries = 0usize;
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
        // Compact older turns; keep the recent ones verbatim.
        let (older, _recent_records) =
            dc_context::split_for_compaction(&history, cfg.keep_recent_turns);
        let summary = summarize_history(older);

        // Assemble the budgeted, zoned prompt (spec 05). The plan rides in the
        // retrieved zone as compact structured state (spec 05).
        let mut segments = vec![
            Segment::system(Zone::System, system.clone()),
            Segment::user(Zone::TaskAnchor, instruction.to_string()),
        ];
        let plan_render = plan.render();
        if !plan_render.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, plan_render));
        }
        // The repo map helps a model navigate to the right file — but a focus-scoped
        // worker is already shown its exact file below, so the map is just noise that
        // tempts a dumb model toward the wrong target. Skip it when focused.
        if !repo_map.is_empty() && cfg.focus_files.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, repo_map.clone()));
        }
        // Pin the current contents of the focused files, re-read fresh every turn so
        // the view never goes stale after an edit (the failure mode that traps a
        // tiny model into re-applying its own first edit). This is the live anchor
        // the model copies `old_str` from.
        let focus = render_focus_files(workspace, &cfg.focus_files);
        if !focus.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, focus));
        }
        if !summary.is_empty() {
            segments.push(Segment::user(Zone::HistorySummary, summary));
        }
        for (i, m) in recent.iter().enumerate() {
            let zone = if i + 1 == recent.len() {
                Zone::RecentObservation
            } else {
                Zone::HistorySummary
            };
            segments.push(seg_from_message(zone, m));
        }

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
        let resp = backend.generate(&req)?;
        // Emit the model's full raw output for this turn (spec 06 — show what the
        // model actually said).
        sink.record(&AgentEvent::ModelTurn {
            step: step + 1,
            prompt_tokens: built.tokens_used,
            raw: resp.content.clone(),
        });

        // Decode the tool call.
        let (obs, action, changed, tool, arg) = match strategy.extract(&resp.content, registry) {
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
                            interventions += 1;
                            stall.reset();
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
                                interventions,
                            ));
                        }
                    }
                } else {
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
                                        interventions,
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
                                        interventions,
                                    });
                                }
                            }
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
            if prev_action == Some(action) && is_idempotent_tool(&tool) {
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
                        interventions += 1;
                        nudge_streak = 0;
                        sink.record(&AgentEvent::Advice {
                            trigger: format!("repeating {tool}"),
                            advice: advice.clone(),
                        });
                        advice
                    }
                    None if tool == "run_verification" => {
                        "You just ran the tests and nothing has changed since — re-running \
                         gives the same result. The suite is still failing: EDIT the code to \
                         fix the reported failure, then run_verification."
                            .to_string()
                    }
                    None => format!(
                        "You already have the result of `{tool}` (it's in the context \
                         above) — re-running it changes nothing. Make the edit the task \
                         needs now with edit_file, then run_verification."
                    ),
                };
                (obs, action, false, tool, arg)
            } else {
                nudge_streak = 0;
                (obs, action, changed, tool, arg)
            };
        prev_action = Some(action);

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
        let write_loop = edit_missed || create_clash;
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
            interventions += 1;
            let directive = if create_clash {
                format!(
                    "`{arg}` already exists — `create_file` will NOT overwrite it, so \
                     repeating it does nothing. To change it, call `write_file` with `path` \
                     `{arg}` and the ENTIRE new file contents in one shot (write_file \
                     overwrites). Make the fix the failing test needs."
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
        history.push(TurnRecord::new(tool, arg, was_error));
        let trimmed = truncate_observation(&obs, cfg.observation_line_cap, true);
        push_recent(&mut recent, &resp.content, &trimmed, cfg.keep_recent_turns);

        // Auto test-repair (spec 03): the moment an edit lands, the harness runs
        // the suite itself — the model shouldn't have to remember to verify. If
        // it's green the task is done (auto-finish); if not, the failures re-enter
        // the loop as a fresh observation the model reacts to.
        if changed {
            if let Some(cmd) = &cfg.verify_command {
                let report = dc_verify::run_verification_in(&cfg.sandbox, workspace, cmd);
                sink.record(&AgentEvent::Verification {
                    green: report.all_green(),
                    summary: first_line(&report.observation()),
                    full: report.observation(),
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
                        interventions,
                    });
                } else {
                    // Surface the failing tests so the next turn is grounded.
                    let fb = format!(
                        "(harness ran the tests after your edit)\n{}",
                        report.observation()
                    );
                    push_observation(
                        &mut recent,
                        &truncate_observation(&fb, cfg.observation_line_cap, true),
                        cfg.keep_recent_turns,
                    );
                    // A failed auto-verify resets the stall streak: real progress
                    // was attempted, so don't count the edit+verify as "stuck".
                    stall.reset();
                }
            }
        }

        match stall.observe(action, changed, cfg.repeat_limit, cfg.no_progress_limit) {
            Progress::Ok => {}
            stuck @ (Progress::Looping | Progress::Stuck) => {
                let trigger = match stuck {
                    Progress::Looping => "repeating the same action without progress",
                    _ => "many turns with no change to the workspace",
                };
                sink.record(&AgentEvent::Stalled {
                    trigger: trigger.to_string(),
                });
                // Junior asks senior for a nudge (spec 02). With no advisor (the
                // single-model setup), the harness steers the model back in-band a
                // bounded number of times before giving up — a capable model just
                // needs a firm directive, not a senior.
                match escalate(advisor, instruction, &plan, &history, trigger) {
                    Some(advice) => {
                        interventions += 1;
                        stall.reset();
                        sink.record(&AgentEvent::Advice {
                            trigger: trigger.to_string(),
                            advice: advice.clone(),
                        });
                        push_observation(&mut recent, &advice, cfg.keep_recent_turns);
                    }
                    None if self_recoveries < SELF_RECOVERY_LIMIT => {
                        self_recoveries += 1;
                        interventions += 1;
                        stall.reset();
                        prev_action = None;
                        let advice = self_recovery_directive(&recent_tools(&history));
                        sink.record(&AgentEvent::Advice {
                            trigger: trigger.to_string(),
                            advice: advice.clone(),
                        });
                        push_observation(&mut recent, &advice, cfg.keep_recent_turns);
                    }
                    None => {
                        let reason = StopReason::Stalled(trigger.to_string());
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
                            interventions,
                        ));
                    }
                }
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
        interventions,
    ))
}

/// How many times the harness steers a stalled model back in-band when there is no
/// advisor to escalate to (the single-model setup). Bounded so a genuinely stuck
/// model still terminates rather than burning the whole step budget looping.
const SELF_RECOVERY_LIMIT: usize = 2;

/// The last few distinct tools the model has used, most-recent first — context for
/// the self-recovery directive so it names what the model keeps doing.
fn recent_tools(history: &[TurnRecord]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in history.iter().rev() {
        if !out.contains(&t.tool) {
            out.push(t.tool.clone());
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

/// A firm, advisor-free recovery instruction injected when a single model stalls.
/// Unlike the gentle repeat-nudge, this names the loop and gives the model a
/// concrete decision: if you've read what you need, EDIT now; if the suite is the
/// blocker, fix the failure it reported. The model has no senior to ask, so the
/// harness has to be the one that breaks the loop.
fn self_recovery_directive(recent: &[String]) -> String {
    let looped = recent
        .first()
        .map(String::as_str)
        .unwrap_or("the same tool");
    format!(
        "STOP — you are stuck in a loop calling `{looped}` and making no progress. \
         You already have everything you read in the context above; re-reading or \
         re-running changes nothing. Decide the next CONCRETE move right now:\n\
         - If the source file the tests need does not exist yet, CREATE it with \
         `edit_file` (write the whole file).\n\
         - If it exists but a test is failing, EDIT it to fix the exact failure the \
         suite reported, then `run_verification`.\n\
         Emit an `edit_file` (or another action that changes the workspace) this turn. \
         Do NOT emit `{looped}` again."
    )
}

/// Consult the advisor (senior) for a hint, formatted as guidance to inject.
/// `None` when there's no advisor or it couldn't help — the caller then stops.
fn escalate(
    advisor: Option<&dyn ModelBackend>,
    task: &str,
    plan: &PlanState,
    history: &[TurnRecord],
    trigger: &str,
) -> Option<String> {
    let advisor = advisor?;
    let recent = summarize_history(history);
    let plan_render = plan.render();
    let advice = consult(
        advisor,
        &Predicament {
            task,
            plan: &plan_render,
            recent: &recent,
            trigger,
        },
    )?;
    Some(advice_observation(&advice))
}

/// Build a non-finished stop report, computing `verified` if a command is set.
#[allow(clippy::too_many_arguments)]
fn stopped(
    reason: StopReason,
    steps: usize,
    sandbox: &dc_verify::Sandbox,
    verify_command: &Option<String>,
    workspace: &Path,
    journal: &Journal,
    metrics: ToolCallMetrics,
    peak_prompt_tokens: usize,
    prompt_budget: usize,
    interventions: usize,
) -> AgentReport {
    AgentReport {
        finished: false,
        steps,
        metrics,
        peak_prompt_tokens,
        prompt_budget,
        verified: verify_command
            .as_ref()
            .map(|c| dc_verify::run_verification_in(sandbox, workspace, c).all_green()),
        change_summary: journal.change_summary(),
        stop_reason: reason,
        interventions,
    }
}

/// Outcome of the whole-suite gate at `finish`.
enum FinishGate {
    /// Finish is honored; the bool is the verified state (None → no verify cmd).
    Allow(Option<bool>),
    /// Finish is refused with an observation the model must react to.
    Refuse(String),
}

/// Run the configured verification before honoring `finish` (spec 11). With no
/// command configured, finish is always allowed (verified = None).
fn gate_finish(
    sandbox: &dc_verify::Sandbox,
    verify_command: &Option<String>,
    workspace: &Path,
) -> FinishGate {
    match verify_command {
        None => FinishGate::Allow(None),
        Some(cmd) => {
            let report = dc_verify::run_verification_in(sandbox, workspace, cmd);
            if report.all_green() {
                FinishGate::Allow(Some(true))
            } else {
                FinishGate::Refuse(format!(
                    "cannot finish yet — the suite is not green:\n{}",
                    report.observation()
                ))
            }
        }
    }
}

/// Execute a validated call: enforce the permission gate (spec 04), then route
/// to the right executor. `find_symbol` goes to the retrieval index and
/// `run_command`/`run_verification` to dc-verify (neither belongs in the pure-fs
/// tool registry); everything else is the registry's `execute`.
// Each parameter is a distinct, irreducible concern of one tool dispatch (the call,
// the registry/policy it's checked against, the confirm seam + its session allowlist,
// the verify command, the dry-run flag, the workspace); bundling them into a struct
// would only move the noise. Private routing fn — keep it flat.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    call: &dc_tools::ValidatedCall,
    registry: &ToolRegistry,
    policy: &PermissionPolicy,
    confirmer: Option<&dyn Confirmer>,
    session_allow: &mut Vec<String>,
    sandbox: &dc_verify::Sandbox,
    verify_command: &Option<String>,
    dry_run: bool,
    workspace: &Path,
) -> ToolOutcome {
    // Permission gate — the harness decides, outside the model's control (spec 04).
    if let Some(spec) = registry.get(&call.name) {
        if let dc_tools::Decision::Deny(reason) = policy.check(call, spec.side_effect) {
            // Only `run_command` is confirm-gated. Other denials (frozen tests, etc.)
            // keep their current auto-deny behavior untouched.
            if call.name == "run_command" {
                let cmd = call.str("command").unwrap_or_default();

                // A command approved-and-remembered earlier this run is already
                // allowed — fall through to execution without re-prompting.
                let remembered = session_allow.iter().any(|p| cmd.starts_with(p.as_str()));
                if !remembered {
                    // A small model often reaches for `run_command "pytest"/"cargo
                    // test"`; redirect it to the allowed run_verification tool instead
                    // of prompting or denying (spec 04 — structured feedback). This
                    // takes precedence over the confirmer.
                    if looks_like_test_command(call.str("command")) {
                        return ToolOutcome::Observation(
                            "run_command denied (shell is blocked). To run the tests, use \
                             {\"tool\":\"run_verification\"} instead."
                                .to_string(),
                        );
                    }
                    // Ask the human, iff a confirmer is wired. No confirmer ⇒ today's
                    // exact behavior: the static Deny stands.
                    match confirmer {
                        None => {
                            return ToolOutcome::Observation(format!(
                                "{} denied: {reason}",
                                call.name
                            ))
                        }
                        Some(c) => match c.confirm_command(cmd, &reason) {
                            Confirmation::Deny(why) => {
                                return ToolOutcome::Observation(format!(
                                    "run_command denied: {why}"
                                ))
                            }
                            Confirmation::AllowRemember { prefix } => session_allow.push(prefix),
                            Confirmation::AllowOnce => {}
                        },
                    }
                }
                // Approved (once, remembered, or matched a remembered prefix): fall
                // through to the shared dry-run check + execution below, so `--dry-run`
                // is still honored for a human-approved command.
            } else {
                return ToolOutcome::Observation(format!("{} denied: {reason}", call.name));
            }
        }

        // Dry-run (spec 06): preview only. Read-only tools still run for real (the
        // model needs true context to reason); any side-effecting tool — edits,
        // create_file, run_command, run_verification — is short-circuited to a note
        // so the workspace is never touched and no process is spawned.
        if dry_run && spec.side_effect != dc_tools::SideEffect::ReadOnly {
            let arg = key_arg(call);
            let target = if arg.is_empty() {
                String::new()
            } else {
                format!(" {arg}")
            };
            return ToolOutcome::Observation(format!(
                "[dry-run] would {}{target}; no changes written",
                call.name
            ));
        }
    }

    match call.name.as_str() {
        "find_symbol" => {
            let name = call.str("name").unwrap_or_default();
            ToolOutcome::Observation(dc_index::find_symbol(workspace, name))
        }
        "run_command" => {
            let cmd = call.str("command").unwrap_or_default();
            let r = dc_verify::run_command(workspace, cmd);
            ToolOutcome::Observation(format!(
                "run_command {cmd:?} exited {}:\n{}",
                r.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                r.output.trim()
            ))
        }
        "run_verification" => match verify_command {
            Some(cmd) => ToolOutcome::Observation(
                dc_verify::run_verification_in(sandbox, workspace, cmd).observation(),
            ),
            None => ToolOutcome::Observation(
                "run_verification: no verification command is configured for this project".into(),
            ),
        },
        _ => execute(call, workspace),
    }
}

/// Append the assistant action + its observation, capping the verbatim window to
/// roughly `keep_recent` turns (each turn is one assistant + one user message).
fn push_recent(recent: &mut Vec<Message>, action: &str, observation: &str, keep_recent: usize) {
    recent.push(Message::assistant(action.to_string()));
    recent.push(Message::user(observation.to_string()));
    trim_recent(recent, keep_recent);
}

/// Inject a harness-originated observation (e.g. advisor advice) as a plain user
/// message — NOT a fake assistant turn, so the model never sees itself "saying"
/// a harness label and parrots it back.
fn push_observation(recent: &mut Vec<Message>, observation: &str, keep_recent: usize) {
    recent.push(Message::user(observation.to_string()));
    trim_recent(recent, keep_recent);
}

fn trim_recent(recent: &mut Vec<Message>, keep_recent: usize) {
    let max_msgs = keep_recent.saturating_mul(2).max(2);
    while recent.len() > max_msgs {
        recent.remove(0);
    }
}

/// The lowercase role word for the verbose prompt dump (`PromptAssembled`).
fn role_word(role: dc_model::Role) -> &'static str {
    match role {
        dc_model::Role::System => "system",
        dc_model::Role::User => "user",
        dc_model::Role::Assistant => "assistant",
    }
}

fn seg_from_message(zone: Zone, m: &Message) -> Segment {
    match m.role {
        dc_model::Role::System => Segment::system(zone, m.content.clone()),
        dc_model::Role::User => Segment::user(zone, m.content.clone()),
        dc_model::Role::Assistant => Segment::assistant(zone, m.content.clone()),
    }
}

/// If `call` is a mutating, path-bearing tool, return its workspace-relative
/// path (so the journal can snapshot it). `run_verification`/`run_command` are
/// mutating-ish but have no single file to record.
fn mutating_path(call: &dc_tools::ValidatedCall, registry: &ToolRegistry) -> Option<String> {
    let spec = registry.get(&call.name)?;
    if spec.side_effect != dc_tools::SideEffect::Mutating {
        return None;
    }
    call.str("path").map(|s| s.to_string())
}

/// The first non-empty line of an observation, for a tight one-line event.
fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Render the current contents of the focused files for the retrieved zone, with
/// line numbers so a small model can copy an exact, unique `old_str`. Re-read from
/// the workspace each turn, so it always reflects edits already made.
fn render_focus_files(workspace: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "The file to edit (copy old_str exactly from between the === markers; this \
         updates after each edit):\n",
    );
    let mut any = false;
    for f in files {
        let p = workspace.join(f);
        if let Ok(content) = std::fs::read_to_string(&p) {
            any = true;
            // Verbatim, with no line numbers — whatever the model copies as old_str
            // must match the file byte-for-byte, so any prefix we add would poison it.
            s.push_str(&format!("\n=== {f} ===\n{content}\n=== end {f} ===\n"));
        }
    }
    if any {
        s
    } else {
        String::new()
    }
}

/// Tools whose result is fully determined by the current workspace + args, so
/// issuing the *same* call twice in a row (with nothing changed between) yields
/// the same observation — used by the repeat-dedup nudge. `run_verification` is
/// included: re-running the suite without an intervening edit can only reprint
/// the same failures, and a tiny model loves to re-verify instead of fixing.
fn is_idempotent_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file" | "list_dir" | "search_code" | "find_symbol" | "run_verification"
    )
}

/// The key argument of a call, for the history record (path or query/name).
fn key_arg(call: &dc_tools::ValidatedCall) -> String {
    for k in ["path", "query", "name"] {
        if let Some(v) = call.str(k) {
            return v.to_string();
        }
    }
    String::new()
}

/// Does an observation read like a failure the model must react to?
/// Does a shell command look like an attempt to run the test suite? Used to
/// redirect a denied `run_command` to `run_verification`.
fn looks_like_test_command(cmd: Option<&str>) -> bool {
    let c = cmd.unwrap_or_default().to_ascii_lowercase();
    c.contains("pytest")
        || c.contains("cargo test")
        || c.contains("npm test")
        || c.contains("go test")
        || (c.contains("test") && c.contains("python"))
}

fn looks_like_failure(obs: &str) -> bool {
    let l = obs.to_ascii_lowercase();
    // A green verification says "all N passed ✓"; a red one says "K failed".
    // "passed" with no "failed" must NOT read as a failure, so check failure
    // markers but exclude the all-passed phrasing.
    if l.contains("passed") && !l.contains("failed") && !l.contains("error") {
        return false;
    }
    l.contains("error")
        || l.contains("rejected")
        || l.contains("not found")
        || l.contains("no match")
        || l.contains("failed")
        || l.contains("exited non-zero")
}

/// Crude identifier extraction from the task text, to boost the repo map toward
/// symbols the user actually named (spec 05). Splits on non-identifier chars and
/// keeps word-ish tokens of length ≥ 3.
fn mentioned_identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush_ident(&mut cur, &mut out);
        }
    }
    flush_ident(&mut cur, &mut out);
    out
}

fn flush_ident(cur: &mut String, out: &mut Vec<String>) {
    if cur.len() >= 3 && !out.contains(cur) {
        out.push(cur.clone());
    }
    cur.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::{CallbackBackend, GenerateResponse, MockBackend};
    use serde_json::json;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-core-agent-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_a_file_then_finishes() {
        let ws = temp_dir("write");
        let backend = MockBackend::new([
            json!({"tool":"write_file","path":"out.txt","content":"hi"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);

        let report = run_agent(&backend, "create out.txt", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.steps, 2);
        assert_eq!(report.metrics.valid, 2);
        assert_eq!(report.metrics.invalid, 0);
        assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "hi");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn verbose_emits_the_assembled_prompt_only_when_enabled() {
        use crate::event::AgentEvent;
        use std::sync::Mutex;

        let registry = dc_tools::default_registry();

        let run = |verbose: bool| -> Vec<AgentEvent> {
            let ws = temp_dir(if verbose { "verbose-on" } else { "verbose-off" });
            let backend = MockBackend::new([json!({"tool":"finish"}).to_string()]);
            let strategy = crate::strategy::select_strategy(&backend.capabilities());
            let evs: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
            let sink = crate::event::FnSink(|e: &AgentEvent| evs.lock().unwrap().push(e.clone()));
            let cfg = AgentConfig {
                verbose,
                ..Default::default()
            };
            run_agent_observed(
                &backend,
                None,
                &registry,
                strategy.as_ref(),
                "x",
                &ws,
                &cfg,
                &sink,
            )
            .unwrap();
            let _ = std::fs::remove_dir_all(&ws);
            evs.into_inner().unwrap()
        };

        // Verbose on: a PromptAssembled event carries the real system prompt content.
        let on = run(true);
        let prompt = on.iter().find_map(|e| match e {
            AgentEvent::PromptAssembled { messages, .. } => Some(messages.clone()),
            _ => None,
        });
        let messages = prompt.expect("verbose run should emit PromptAssembled");
        assert!(
            messages.iter().any(|m| m.role == "system"),
            "the assembled prompt includes the system message: {messages:?}"
        );

        // Verbose off (default): no PromptAssembled events at all.
        let off = run(false);
        assert!(
            !off.iter()
                .any(|e| matches!(e, AgentEvent::PromptAssembled { .. })),
            "no prompt dump without --verbose"
        );
    }

    #[test]
    fn dry_run_previews_mutations_without_touching_the_workspace() {
        use crate::event::AgentEvent;
        use std::sync::Mutex;

        let ws = temp_dir("dry-run");
        std::fs::write(ws.join("f.txt"), "ORIGINAL").unwrap();

        // Turn 1: read the file (read-only — must run for real so the model sees it).
        // Turn 2: try to overwrite it (mutating — must be previewed, not applied).
        // Turn 3: finish.
        let backend = MockBackend::new([
            json!({"tool":"read_file","path":"f.txt"}).to_string(),
            json!({"tool":"write_file","path":"f.txt","content":"CLOBBERED"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);

        let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
        let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
        let registry = dc_tools::default_registry();
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let cfg = AgentConfig {
            dry_run: true,
            ..Default::default()
        };
        let report = run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            "edit f.txt",
            &ws,
            &cfg,
            &sink,
        )
        .unwrap();
        assert!(report.finished);

        // The mutating tool never wrote: the file is byte-for-byte the original.
        assert_eq!(
            std::fs::read_to_string(ws.join("f.txt")).unwrap(),
            "ORIGINAL"
        );

        let evs = events.lock().unwrap();
        // The read returned the *real* content (read-only tools still run).
        assert!(
            evs.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult { full, .. } if full.contains("ORIGINAL")
            )),
            "read_file should return the real file body in dry-run: {evs:?}"
        );
        // The write produced a [dry-run] preview note instead of applying.
        assert!(
            evs.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult { summary, .. } if summary.contains("[dry-run]")
            )),
            "write_file should be previewed with a [dry-run] note: {evs:?}"
        );

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn recovers_from_a_malformed_tool_call() {
        let ws = temp_dir("repair");
        // First turn is garbage; the loop must feed back an error and continue.
        let backend = MockBackend::new([
            "not json at all".to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);

        let report = run_agent(&backend, "do it", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.steps, 2);
        // One invalid (the garbage), one valid (the finish).
        assert_eq!(report.metrics.invalid, 1);
        assert_eq!(report.metrics.valid, 1);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_schema_violation_is_repaired_not_executed() {
        let ws = temp_dir("schema-repair");
        // read_file without a path is valid JSON but invalid against the schema;
        // it must be fed back, not executed, then the model recovers.
        let backend = MockBackend::new([
            json!({"tool":"read_file"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);
        let report = run_agent(&backend, "x", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.metrics.invalid, 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn stops_at_the_step_budget() {
        let ws = temp_dir("budget");
        // A backend that never finishes: always asks to read the same file.
        let read = json!({"tool":"read_file","path":"x"}).to_string();
        let backend = CallbackBackend::android_core(move |_req| {
            Ok(GenerateResponse {
                content: read.clone(),
            })
        });

        let cfg = AgentConfig {
            max_steps: 3,
            ..Default::default()
        };
        let report = run_agent(&backend, "loop forever", &ws, &cfg).unwrap();
        assert!(!report.finished);
        assert_eq!(report.steps, 3);
        assert_eq!(report.metrics.valid, 3);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn propagates_backend_errors() {
        let ws = temp_dir("err");
        let backend = MockBackend::new(Vec::<String>::new()); // exhausts immediately
        assert!(run_agent(&backend, "x", &ws, &AgentConfig::default()).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_repeated_read_is_nudged_not_re_served() {
        use crate::event::AgentEvent;
        use std::sync::Mutex;

        let ws = temp_dir("read-dedup");
        std::fs::write(ws.join("f.txt"), "FILE_BODY_MARKER").unwrap();

        // The model reads the same file twice, then finishes. The second read must
        // come back as a nudge — not the file body again.
        let backend = MockBackend::new([
            json!({"tool":"read_file","path":"f.txt"}).to_string(),
            json!({"tool":"read_file","path":"f.txt"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);

        #[derive(Default)]
        struct Rec {
            results: Mutex<Vec<String>>,
        }
        impl crate::event::EventSink for Rec {
            fn record(&self, e: &AgentEvent) {
                if let AgentEvent::ToolResult { full, .. } = e {
                    self.results.lock().unwrap().push(full.clone());
                }
            }
        }

        let registry = dc_tools::default_registry();
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let sink = Rec::default();
        let report = run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            "read it",
            &ws,
            &AgentConfig::default(),
            &sink,
        )
        .unwrap();
        assert!(report.finished);

        let results = sink.results.lock().unwrap();
        // First read returns the file body; the second is the de-dup nudge.
        assert!(
            results[0].contains("FILE_BODY_MARKER"),
            "first read serves the file: {:?}",
            results[0]
        );
        assert!(
            results[1].contains("already have the result")
                && !results[1].contains("FILE_BODY_MARKER"),
            "second identical read is nudged, not re-served: {:?}",
            results[1]
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn no_advisor_self_recovers_before_giving_up() {
        use crate::event::AgentEvent;
        use std::sync::Mutex;

        let ws = temp_dir("self-recover");
        std::fs::write(ws.join("f.txt"), "BODY").unwrap();

        // A model that loops on the same read forever, with NO advisor. The harness
        // must steer it back in-band (emit Advice) at each stall instead of stopping
        // on the first one — but still terminate once the recovery budget is spent.
        let read = json!({"tool":"read_file","path":"f.txt"}).to_string();
        let backend = CallbackBackend::android_core(move |_req| {
            Ok(GenerateResponse {
                content: read.clone(),
            })
        });

        #[derive(Default)]
        struct Adv {
            advice: Mutex<Vec<String>>,
            stalled: Mutex<usize>,
        }
        impl crate::event::EventSink for Adv {
            fn record(&self, e: &AgentEvent) {
                match e {
                    AgentEvent::Advice { advice, .. } => {
                        self.advice.lock().unwrap().push(advice.clone())
                    }
                    AgentEvent::Stalled { .. } => *self.stalled.lock().unwrap() += 1,
                    _ => {}
                }
            }
        }

        let registry = dc_tools::default_registry();
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let sink = Adv::default();
        let cfg = AgentConfig {
            max_steps: 30,
            ..Default::default()
        };
        let report = run_agent_observed(
            &backend,
            None, // no advisor — the single-model setup
            &registry,
            strategy.as_ref(),
            "read forever",
            &ws,
            &cfg,
            &sink,
        )
        .unwrap();

        // It eventually gives up (the model never edits), but only AFTER self-recovery.
        assert!(!report.finished);
        assert!(
            matches!(report.stop_reason, StopReason::Stalled(_)),
            "should stop stalled, got {:?}",
            report.stop_reason
        );
        // SELF_RECOVERY_LIMIT firm directives were injected before giving up.
        let advice = sink.advice.lock().unwrap();
        assert_eq!(
            advice.len(),
            SELF_RECOVERY_LIMIT,
            "expected {SELF_RECOVERY_LIMIT} self-recovery directives, got {advice:?}"
        );
        assert!(
            advice[0].contains("stuck in a loop") && advice[0].contains("edit_file"),
            "directive names the loop and points at the edit: {:?}",
            advice[0]
        );
        // It did NOT die on the first stall: more stalls than the no-advisor stop
        // would have allowed (1).
        assert!(*sink.stalled.lock().unwrap() > 1);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn repeated_edit_miss_is_steered_to_write_file() {
        use crate::event::AgentEvent;
        use std::sync::Mutex;

        let ws = temp_dir("edit-loop");
        // The file exists but does NOT contain the model's imagined anchor, so every
        // edit_file misses. After two misses the harness must steer it to write_file.
        std::fs::write(ws.join("app.py"), "x = 1\n").unwrap();

        let miss = json!({"tool":"edit_file","path":"app.py",
            "old_str":"return jsonify(x)","new_str":"return jsonify(x), 200"})
        .to_string();
        let backend = MockBackend::new([
            miss.clone(),
            miss.clone(),
            miss, // 3 misses
            json!({"tool":"finish"}).to_string(),
        ]);

        #[derive(Default)]
        struct Cap {
            advice: Mutex<Vec<String>>,
        }
        impl crate::event::EventSink for Cap {
            fn record(&self, e: &AgentEvent) {
                if let AgentEvent::Advice { advice, .. } = e {
                    self.advice.lock().unwrap().push(advice.clone());
                }
            }
        }

        let registry = dc_tools::default_registry();
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let sink = Cap::default();
        let _ = run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            "fix it",
            &ws,
            &AgentConfig::default(),
            &sink,
        )
        .unwrap();

        let advice = sink.advice.lock().unwrap();
        assert!(
            advice.iter().any(|a| a.contains("write_file")
                && a.contains("anchor does not exist")
                && a.contains("app.py")),
            "a repeated edit miss must steer to write_file: {advice:?}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn repeated_create_file_clash_is_steered_to_write_file() {
        use crate::event::AgentEvent;
        use std::sync::Mutex;

        let ws = temp_dir("create-loop");
        // app.py already exists. The model keeps calling create_file to "fix" it, but
        // create_file refuses to overwrite — so it would loop forever. After two clashes
        // the harness must steer it to write_file (observed live: the multi-file db task).
        std::fs::write(ws.join("app.py"), "x = 1\n").unwrap();

        let clash = json!({"tool":"create_file","path":"app.py","content":"y = 2\n"}).to_string();
        let backend = MockBackend::new([
            clash.clone(),
            clash.clone(),
            clash,
            json!({"tool":"finish"}).to_string(),
        ]);

        #[derive(Default)]
        struct Cap {
            advice: Mutex<Vec<String>>,
        }
        impl crate::event::EventSink for Cap {
            fn record(&self, e: &AgentEvent) {
                if let AgentEvent::Advice { advice, .. } = e {
                    self.advice.lock().unwrap().push(advice.clone());
                }
            }
        }

        let registry = dc_tools::default_registry();
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let sink = Cap::default();
        let _ = run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            "fix it",
            &ws,
            &AgentConfig::default(),
            &sink,
        )
        .unwrap();

        let advice = sink.advice.lock().unwrap();
        assert!(
            advice.iter().any(|a| a.contains("write_file")
                && a.contains("already exists")
                && a.contains("app.py")),
            "a repeated create_file clash must steer to write_file: {advice:?}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    // --- Confirm-gated run_command (spec 04 / spec 06) -----------------------

    use crate::confirm::{Confirmation, Confirmer};
    use std::sync::Mutex;

    /// Records every command it's asked about and answers with a canned decision.
    struct FakeConfirmer {
        answer: Confirmation,
        seen: Mutex<Vec<String>>,
    }
    impl FakeConfirmer {
        fn new(answer: Confirmation) -> Self {
            Self {
                answer,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }
    impl Confirmer for FakeConfirmer {
        fn confirm_command(&self, command: &str, _default_reason: &str) -> Confirmation {
            self.seen.lock().unwrap().push(command.to_string());
            self.answer.clone()
        }
    }

    fn run_command_call(cmd: &str) -> dc_tools::ValidatedCall {
        let mut args = std::collections::BTreeMap::new();
        args.insert("command".to_string(), json!(cmd));
        dc_tools::ValidatedCall {
            name: "run_command".to_string(),
            args,
        }
    }

    /// `dispatch` with the default (shell-denying) policy, a temp workspace, and a
    /// caller-supplied confirmer + session allowlist. Returns the observation text.
    fn dispatch_run_command(
        cmd: &str,
        confirmer: Option<&dyn Confirmer>,
        session_allow: &mut Vec<String>,
        dry_run: bool,
    ) -> String {
        let ws = temp_dir("confirm");
        let registry = dc_tools::default_registry();
        let policy = PermissionPolicy::default(); // shell denied
        let outcome = dispatch(
            &run_command_call(cmd),
            &registry,
            &policy,
            confirmer,
            session_allow,
            &dc_verify::Sandbox::Host,
            &None,
            dry_run,
            &ws,
        );
        let _ = std::fs::remove_dir_all(&ws);
        match outcome {
            ToolOutcome::Observation(s) => s,
            _ => panic!("expected an Observation from run_command dispatch"),
        }
    }

    #[test]
    fn unapproved_shell_denied_when_no_confirmer() {
        // No confirmer ⇒ today's behavior: the static Deny stands.
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", None, &mut allow, false);
        assert!(obs.contains("denied"), "{obs}");
        assert!(!obs.contains("exited"), "command must not run: {obs}");
        assert!(allow.is_empty());
    }

    #[test]
    fn confirmer_allow_once_runs_otherwise_denied_command() {
        let fake = FakeConfirmer::new(Confirmation::AllowOnce);
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", Some(&fake), &mut allow, false);
        assert!(obs.contains("exited"), "command should have run: {obs}");
        assert_eq!(fake.calls(), 1);
        assert!(allow.is_empty(), "AllowOnce must not remember anything");
    }

    #[test]
    fn confirmer_deny_blocks_command() {
        let fake = FakeConfirmer::new(Confirmation::Deny("nope".to_string()));
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", Some(&fake), &mut allow, false);
        assert!(obs.contains("denied: nope"), "{obs}");
        assert!(!obs.contains("exited"), "command must not run: {obs}");
    }

    #[test]
    fn remember_mutates_effective_allowlist_for_rest_of_run() {
        let fake = FakeConfirmer::new(Confirmation::AllowRemember {
            prefix: "echo ".to_string(),
        });
        let mut allow = Vec::new();

        // First matching command: prompts once, runs, and remembers the prefix.
        let first = dispatch_run_command("echo one", Some(&fake), &mut allow, false);
        assert!(first.contains("exited"), "{first}");
        assert_eq!(allow, vec!["echo ".to_string()]);

        // Second matching command: runs WITHOUT consulting the confirmer again.
        let second = dispatch_run_command("echo two", Some(&fake), &mut allow, false);
        assert!(second.contains("exited"), "{second}");
        assert_eq!(
            fake.calls(),
            1,
            "remembered prefix must short-circuit the gate"
        );
    }

    #[test]
    fn test_command_redirect_still_wins_over_confirmer() {
        // The pytest→run_verification redirect precedes prompting, so the confirmer
        // is never consulted for a test command.
        let fake = FakeConfirmer::new(Confirmation::AllowOnce);
        let mut allow = Vec::new();
        let obs = dispatch_run_command("pytest", Some(&fake), &mut allow, false);
        assert!(obs.contains("run_verification"), "{obs}");
        assert_eq!(
            fake.calls(),
            0,
            "confirmer must not be consulted for a test cmd"
        );
    }

    #[test]
    fn dry_run_honored_even_when_confirmer_allows() {
        // A human-approved command still respects --dry-run: no process is spawned.
        let fake = FakeConfirmer::new(Confirmation::AllowOnce);
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", Some(&fake), &mut allow, true);
        assert!(obs.contains("[dry-run]"), "{obs}");
        assert!(!obs.contains("exited"), "dry-run must not execute: {obs}");
    }
}
