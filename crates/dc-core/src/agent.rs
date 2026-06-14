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

use dc_context::{
    prompt_budget, summarize_history, truncate_observation, ContextBuilder, Segment, TokenCounter,
    TurnRecord, Zone,
};
use dc_index::Boosts;
use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;
use dc_tools::{execute, Journal, PermissionPolicy, ToolOutcome, ToolRegistry};

use crate::advisor::{advice_observation, consult, Predicament};
use crate::event::{AgentEvent, EventSink, NullSink};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::planner::make_plan;
use crate::recovery::{action_hash, Progress, StallDetector, StopReason};
use crate::strategy::ToolCallStrategy;

/// Loop configuration, including the Context Manager's budget knobs (spec 05).
#[derive(Debug, Clone)]
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
    let mut system = format!("{TASK_PREFIX}{}", strategy.system_preamble(registry));
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
        if !repo_map.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, repo_map.clone()));
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
                        &cfg.verify_command,
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
                            match gate_finish(&cfg.verify_command, workspace) {
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
                                sink.record(&AgentEvent::Verification {
                                    green: !looks_like_failure(&o),
                                    summary: first_line(&o),
                                    full: o.clone(),
                                });
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
                // Junior asks senior for a nudge (spec 02). No advisor → stop.
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
                    None => {
                        let reason = StopReason::Stalled(trigger.to_string());
                        sink.record(&AgentEvent::Stopped {
                            reason: reason.clone(),
                        });
                        return Ok(stopped(
                            reason,
                            step + 1,
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
        &cfg.verify_command,
        workspace,
        &journal,
        metrics,
        peak_prompt_tokens,
        budget,
        interventions,
    ))
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
            .map(|c| dc_verify::run_verification(workspace, c).all_green()),
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
fn gate_finish(verify_command: &Option<String>, workspace: &Path) -> FinishGate {
    match verify_command {
        None => FinishGate::Allow(None),
        Some(cmd) => {
            let report = dc_verify::run_verification(workspace, cmd);
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
fn dispatch(
    call: &dc_tools::ValidatedCall,
    registry: &ToolRegistry,
    policy: &PermissionPolicy,
    verify_command: &Option<String>,
    workspace: &Path,
) -> ToolOutcome {
    // Permission gate — the harness decides, outside the model's control (spec 04).
    if let Some(spec) = registry.get(&call.name) {
        if let dc_tools::Decision::Deny(reason) = policy.check(call, spec.side_effect) {
            // A small model often reaches for `run_command "pytest"/"cargo test"`;
            // redirect it to the allowed run_verification tool instead of just
            // denying (spec 04 — structured, actionable feedback).
            if call.name == "run_command" && looks_like_test_command(call.str("command")) {
                return ToolOutcome::Observation(
                    "run_command denied (shell is blocked). To run the tests, use \
                     {\"tool\":\"run_verification\"} instead."
                        .to_string(),
                );
            }
            return ToolOutcome::Observation(format!("{} denied: {reason}", call.name));
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
            Some(cmd) => {
                ToolOutcome::Observation(dc_verify::run_verification(workspace, cmd).observation())
            }
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
}
