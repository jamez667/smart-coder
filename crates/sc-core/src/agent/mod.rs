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

use sc_context::{
    prompt_budget, truncate_observation, truncate_paged_read, ContextBuilder, TokenCounter,
    TurnRecord,
};
use sc_index::Boosts;
use sc_model::{GenerateRequest, Message, ModelBackend};
use sc_proto::Result;
use sc_tools::{Journal, ToolOutcome, ToolRegistry};

use config::{task_prefix_for, FOCUS_TASK_PREFIX, INVESTIGATE_TASK_PREFIX, TASK_PREFIX_SHELL};
pub use config::{AgentConfig, AgentReport};
pub use dispatch::ExternalTool;

use crate::event::{AgentEvent, EventSink, FaultKind, NullSink};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::planner::make_plan;
use crate::recovery::{action_hash, failure_signature, StallDetector, StopReason};
use crate::strategy::ToolCallStrategy;
use crate::text::{first_line, mentioned_identifiers};

/// Run the agent against `instruction` in `workspace` with the default registry,
/// choosing the strongest tool-call strategy the backend can enforce (spec 02).
/// The name of a tool the prompt steers toward but the registry does not offer.
///
/// Deliberately narrow. It scans only for the BUILT-IN tool names, and only where
/// they appear in backticks -- the house style for naming a tool the model should
/// call. A bare word like "write" or "finish" is ordinary English and would fire on
/// every prompt; the count then becomes noise people learn to ignore, which is worse
/// than no detector at all.
///
/// User text is excluded for the same reason: an instruction that happens to say
/// `edit_lines` is the user's business, not a harness fault. Only the system prompt
/// and harness-authored guidance are checked.
fn unoffered_tool_mentioned(messages: &[Message], registry: &ToolRegistry) -> Option<String> {
    messages
        .iter()
        // Only harness-authored text. A user instruction naming a tool is not our bug.
        .filter(|m| m.role != sc_model::Role::User)
        .find_map(|m| unoffered_tool_in(&m.content, registry))
}

/// [`unoffered_tool_mentioned`] for one harness-authored string: the first built-in tool
/// it names in backticks that `registry` does not offer.
fn unoffered_tool_in(text: &str, registry: &ToolRegistry) -> Option<String> {
    // Every tool the harness knows how to build, taken from the DEFAULT registry
    // rather than a hand-kept list.
    //
    // The list used to be hardcoded here and had drifted: it was missing
    // `search_code`, `read_function`, `edit_function`, `find_symbol`,
    // `append_file` and `create_file`, so guidance naming any of those against a
    // trimmed registry was invisible to this detector -- which is exactly what it
    // exists to catch. Deriving it means a new tool is covered the day it is added.
    let known = sc_tools::default_registry();
    known
        .specs()
        .iter()
        .map(|s| s.name)
        .filter(|name| registry.get(name).is_none())
        .find(|name| text.contains(&format!("`{name}`")))
        .map(str::to_string)
}

/// Check a directive the harness is about to inject, at the point it is injected, and
/// report a `ToolNotOffered` fault if it names a tool this run cannot call.
///
/// The step-0 scan over the assembled prompt covers the static guidance; this covers the
/// strings composed mid-run (a steer after a failed edit, a self-recovery directive), which
/// are exactly the ones that used to hardcode a tool name. Cheap: one short string, only on
/// the turns that inject one.
pub(super) fn report_if_unoffered(
    text: &str,
    registry: &ToolRegistry,
    step: usize,
    sink: &dyn EventSink,
) {
    if let Some(missing) = unoffered_tool_in(text, registry) {
        sink.record(&AgentEvent::HarnessFault {
            kind: FaultKind::ToolNotOffered,
            detail: format!(
                "a harness directive steers the model toward `{missing}`, which is not in \
                 this run's registry; it cannot call it"
            ),
            step: step + 1,
        });
    }
}

pub fn run_agent(
    backend: &dyn ModelBackend,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    let registry = sc_tools::default_registry();
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
    // A registry with nothing mutating in it IS a read-only run, so it needs the prompt that
    // matches. Derived from the registry rather than a config flag: the tools the model can
    // actually call are the ground truth, and a flag would be a second place to keep in sync
    // that could disagree with them.
    let read_only_run = registry
        .specs()
        .iter()
        .all(|s| s.side_effect == sc_tools::SideEffect::ReadOnly);
    // Shell is derived from the REGISTRY, not from the permission flag.
    //
    // `allow_shell` says the policy would permit `run_command`; it does not say
    // the tool is on the menu. When those disagree the prompt names a tool the
    // model cannot call, and a model does what it is told — the same failure
    // this file already documents for `INVESTIGATE_TASK_PREFIX`, where a build
    // prompt over a read-only registry sent a model hunting for `edit_file` and
    // dumping prose for four turns.
    //
    // Measured here too: an experimental registry offering `ask` instead of
    // `read_file`/`run_command` still received TASK_PREFIX_SHELL ("1) run_command
    // to investigate"), and spent ~2 extra turns per task looking for a tool it
    // did not have — a uniform ~5,000-token overhead on tasks whose entire
    // fixture is 1.4 KB.
    let has_shell = registry.get("run_command").is_some();
    let build_prefix;
    let prefix = if read_only_run {
        INVESTIGATE_TASK_PREFIX
    } else if !cfg.focus_files.is_empty() {
        FOCUS_TASK_PREFIX
    } else if cfg.permission.allow_shell && has_shell {
        TASK_PREFIX_SHELL
    } else {
        // Names the tools this registry actually has, rather than assuming
        // read_file/edit_file.
        build_prefix = task_prefix_for(registry);
        build_prefix.as_str()
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
    // Charge the request for what rides outside the messages: native tool schemas
    // are sent as `tools`, which the server tokenizes into the same window.
    let overhead_text = strategy.request_overhead_text(registry);
    let request_overhead = if overhead_text.is_empty() {
        0
    } else {
        counter.count(&overhead_text)
    };
    let builder = ContextBuilder::new(&counter, budget).with_fixed_overhead(request_overhead);

    // The repo map is stable retrieval; boost task-named symbols (spec 05, aider).
    let repo_map = sc_index::repo_map(
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

    // A zero or nonsensical budget means the run is doomed before the first turn.
    //
    // `prompt_budget` is `max_context * fraction - response_reserve`, saturating. If
    // the reserve exceeds the usable window the result is ZERO -- and a zero budget
    // does not fail loudly, it just stops constraining anything: nothing is evicted
    // because there is no ceiling to evict toward, and the prompt grows until the
    // SERVER rejects it. Seen as "request (33164 tokens) exceeds the available
    // context size (32768)" -- a backend error that reads as a model problem and is
    // entirely the harness's doing, caused by a backend left at its 8192 default
    // while the config reserved 12288 for the reply.
    if budget == 0 {
        sink.record(&AgentEvent::HarnessFault {
            kind: FaultKind::ContextBudgetUnusable,
            detail: format!(
                "prompt budget is 0: the model reports a {}-token context, of which \
                 {:.0}% is usable, but {} are reserved for the reply. Nothing will be \
                 evicted and the request will grow until the server rejects it. Detect \
                 the server's real context, or lower `response_reserve_tokens`.",
                caps.max_context_tokens,
                cfg.effective_context_fraction * 100.0,
                cfg.response_reserve_tokens
            ),
            step: 0,
        });
    }

    // Nothing to work from. A blank instruction still runs -- the model is asked to
    // act on whitespace, guesses, and the guesses read as incompetence. Named here,
    // once, before the first turn is spent on it.
    if instruction.trim().is_empty() {
        sink.record(&AgentEvent::HarnessFault {
            kind: FaultKind::EmptyGuidance,
            detail: String::from(
                "the task instruction is blank; the model has nothing to act on and \
                 every turn it spends is guessing",
            ),
            step: 0,
        });
    } else if system.trim().is_empty() {
        sink.record(&AgentEvent::HarnessFault {
            kind: FaultKind::EmptyGuidance,
            detail: String::from(
                "the assembled system prompt is empty; the model was told nothing about \
                 its tools or how to reply",
            ),
            step: 0,
        });
    }

    // A pinned focus file the harness cannot read. `render_focus_files` skips it
    // silently, so the model is told "the file to edit" and shown nothing -- never
    // the model's fault by construction. Test files were once delivered as
    // DIRECTORIES across 185 instances, and every run read them as failed and moved
    // on. One fault per path, at run start, so a wrong path is blamed on the caller
    // that pinned it.
    for f in &cfg.focus_files {
        if let Err(e) = std::fs::read_to_string(workspace.join(f)) {
            sink.record(&AgentEvent::HarnessFault {
                kind: FaultKind::UnreadablePath,
                detail: format!(
                    "focus file `{f}` cannot be read ({e}); it was pinned by the harness, \
                     so the model will be steered toward a file it cannot see"
                ),
                step: 0,
            });
        }
    }

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

    // The retrieved-zone material, rendered ONCE and refreshed only on a workspace change
    // (and then only the parts whose source moved), so the prompt prefix is byte-stable
    // between turns and the backend's KV cache holds. Built after planning, which is the
    // repo map's other consumer.
    let mut stable = StableContext::new(workspace, cfg, registry, instruction, repo_map);

    let mut metrics = ToolCallMetrics::default();
    let mut history: Vec<TurnRecord> = Vec::new();
    let mut recent = RecentWindow::default();
    // How many of the oldest turns have been evicted from `recent` into the history summary.
    // Every window turn has exactly one `history` record (pushed the same iteration), so
    // `history[..evicted_turns]` is precisely the evicted set and the summary changes only
    // when an eviction happens.
    let mut evicted_turns = 0usize;
    let mut peak_prompt_tokens = 0usize;
    // Cumulative prompt tokens across every turn. The PEAK says whether a single
    // prompt fit the window; this says what the whole task cost, which is the
    // number that separates a run that read the right file once from one that
    // re-read four files six times.
    let mut total_prompt_tokens = 0usize;
    // The same total split by what the SERVER had to do with it: how many of those
    // tokens it served from its KV cache, and how many it re-prefilled. The
    // append-only prompt exists to move tokens from the second into the first, and
    // `total_prompt_tokens` is blind to that move -- it counts what we sent, which
    // is identical either way. Both stay 0 on a backend that reports no split.
    let mut total_cached_prompt_tokens = 0usize;
    let mut total_prefilled_prompt_tokens = 0usize;
    // Largest reply seen, so the reply reserve can be checked against reality.
    let mut peak_reply_tokens = 0usize;
    let mut journal = Journal::new();
    let mut stall_detector = StallDetector::default();
    // The harness's in-loop intervention bookkeeping: the running intervention count and the
    // bounded diagnosis/advisor/self-recovery counters the ladder spends (spec 02/03). See
    // [`stall::Interventions`].
    let mut interv = stall::Interventions::default();
    // A failing `edit_file` on this path, and how many times in a row. A small model
    // often anchors `edit_file` on code it *imagines* it wrote (e.g. a `jsonify(...)`
    // line that isn't in the file), so the anchor never matches and it loops. After a
    // couple of misses the harness tells it to stop fiddling with anchors and rewrite
    // the whole (small) file with `create_file` — far more reliable than a perfect
    // anchor. Observed live 2026-06-15 (the A/B `/sum` 500→400 fix it couldn't apply).
    let mut failed_edit_path: Option<String> = None;
    let mut failed_edit_streak = 0usize;
    // Consecutive turns whose reply didn't parse into a tool call at all (malformed JSON). A
    // coder model encoding a long multi-line `edit_file` `old_str` often produces JSON the
    // parser can't extract, and re-tries the SAME malformed call — the anchor-miss breaker never
    // fires (the call never parsed). After a couple, steer it to `edit_lines`, which takes line
    // NUMBERS and no big `old_str`, sidestepping the encoding problem (observed live 2026-07-15).
    let mut malformed_streak = 0usize;
    // Reads before a read-only run is FORCED to answer (the grammar is narrowed to `finish`
    // below). A count of reads over the WHOLE run: on a read-only run nothing ever changes
    // the workspace, so "reads since a change" would be the same number under a name that
    // invites resetting it.
    const FORCE_FINISH_AFTER: usize = 11;
    let mut total_reads = 0usize;
    // How many replies this run have run to the token cap. Each costs a full prompt pass
    // plus a maximum-length generation -- most of a minute on a 35B model -- and carries no
    // tool call, so a run that keeps hitting it is burning time whether or not the waste is
    // contiguous.
    let mut capped_replies = 0usize;
    // The auto-verify failure signature and how many consecutive verifications have carried
    // it. An edit is only progress if it moves the suite: once the same failure has come
    // back this many times in a row, the byte changes are noise and the stall detector is
    // told so (observed live 2026-06-15: ~10 verifications on an unchanged failure, every
    // edit resetting the stall, the run dying at the step budget instead of escalating).
    const UNCHANGED_FAILURE_LIMIT: usize = 3;
    let mut last_failure_sig: Option<u64> = None;
    let mut failure_sig_streak = 0usize;
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
                total_prompt_tokens,
                total_cached_prompt_tokens,
                total_prefilled_prompt_tokens,
                peak_reply_tokens,
                harness_faults: runlog.lock().fault_counts(),
                prompt_budget: budget,
                verified: None,
                change_summary: journal.change_summary(),
                stop_reason: StopReason::Cancelled,
                interventions: interv.count,
            });
        }
        // Assemble the budgeted, zoned prompt (spec 05): the cached retrieval, the plan, the
        // summary of the evicted turns and the sacred recent window; note which files are
        // pinned in full this turn.
        //
        // The recent window is bounded by BUDGET, not by a message count. If the builder had
        // to drop or clip anything to fit, the oldest whole turn is evicted from the window
        // into the compacted summary and the prompt is rebuilt, until it fits or the window
        // is down to `keep_recent_turns` (the verbatim minimum). Evicting from the FRONT is
        // what keeps the prompt append-only between turns: nothing after the summary moves
        // unless a turn actually leaves.
        let (built, pinned_full_files) = loop {
            let (segments, pinned) = assemble::assemble_segments(
                cfg,
                instruction,
                &system,
                &stable,
                &plan,
                &history[..evicted_turns],
                &recent,
            );
            // The builder never adds text: fewer chars out than in means it dropped a zone
            // or clipped a sacred segment -- the prompt did not fit as assembled.
            let raw_chars: usize = segments.iter().map(|s| s.text.len()).sum();
            let built = builder.build(segments);
            let built_chars: usize = built.messages.iter().map(|m| m.content.len()).sum();
            let over = built.tokens_used > built.budget || built_chars < raw_chars;
            if over && recent.len() > cfg.keep_recent_turns.max(1) {
                recent.evict_oldest();
                evicted_turns += 1;
                continue;
            }
            break (built, pinned);
        };
        peak_prompt_tokens = peak_prompt_tokens.max(built.tokens_used);
        total_prompt_tokens += built.tokens_used;

        // The assembled prompt did not fit, and there was nothing left to drop.
        //
        // The builder evicts non-sacred zones and then truncates the truncatable
        // sacred ones; reaching here means even the irreducible content is over
        // budget. Sending it anyway gets an HTTP 400 that costs the whole turn and
        // reads as the model failing -- measured: 26,516 estimated, 34,237 counted
        // by the server, request rejected. Say so instead, on the turn it happens.
        if built.tokens_used > built.budget {
            sink.record(&AgentEvent::HarnessFault {
                kind: FaultKind::PromptOverBudget,
                detail: format!(
                    "the assembled prompt is {} tokens against a {}-token budget and \
                     could not be shrunk further; the backend may reject this turn. \
                     Raise the model's context, or lower `response_reserve_tokens`.",
                    built.tokens_used, built.budget
                ),
                step: step + 1,
            });
        }

        // Did WE just tell the model to use a tool it does not have?
        //
        // Checked on the assembled prompt, which is exactly what the model will see,
        // so it catches the guidance wherever it came from -- a system prompt, a
        // repair message, a focus-file preamble -- rather than one site at a time.
        // Only on the first turn: the prompt's guidance is the same every turn, and a
        // fault repeated forty times is a fault nobody reads.
        if step == 0 {
            if let Some(missing) = unoffered_tool_mentioned(&built.messages, registry) {
                sink.record(&AgentEvent::HarnessFault {
                    kind: FaultKind::ToolNotOffered,
                    detail: format!(
                        "the prompt steers the model toward `{missing}`, which is not in \
                         this run's registry; it cannot call it"
                    ),
                    step: step + 1,
                });
            }
        }

        // A REPEATED TRUNCATED REPLY IS A LOOP. Kill the run.
        //
        // The first version of this compared PROMPTS and was dead code: every failed turn
        // appends the model's reply and the repair error to the history, so the next prompt
        // is never byte-identical and the check could never fire. Measured on a real
        // transcript -- five consecutive prompts, zero identical.
        //
        // What actually repeats is the FAILURE: the model runs to the token cap producing
        // prose with no tool call, gets a repair prompt, and does it again. Each of those
        // turns costs a full ~12k-token prompt pass plus a maximum-length generation -- most
        // of a minute on a 35B model -- and buys nothing. Two in a row is enough: a model
        // that has just been told exactly what went wrong and hit the cap again is not going
        // to converge on the third attempt.
        //
        // Tracked here rather than in the stall detector because that one observes ACTIONS,
        // and these turns produce none.

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
        // The reply budget the prompt was sized against (spec 05) must also be the
        // reply budget the request asks for. It was subtracted from the prompt but
        // never applied here, so every turn silently used `GenerateRequest`'s 1024
        // default however `response_reserve_tokens` was configured.
        //
        // That truncates a REASONING model mid-thought. Measured on Tiel-35B-A3B,
        // five identical single-edit requests at max_tokens 1024: four emitted the
        // call after 131-289 completion tokens, the fifth ran to the cap and returned
        // NOTHING (`finish_reason: length`, no tool_calls). A truncated turn produces
        // no call at all, which the loop reads as the model declining to act -- so a
        // model that reasons long looks like a model that will not edit. Reads are
        // short and always survived; edits carry the reasoning plus an old_str/new_str
        // payload and did not.
        req.max_tokens = if read_only_run {
            // A READ-ONLY turn is a tiny JSON object -- `{"tool":"read_file","path":"..."}`
            // is well under 100 tokens, and the one long reply is `finish`, whose answer runs
            // to ~2,000 characters. Nothing here needs 12,288 tokens.
            //
            // The reserve stays where it is because it also sets the PROMPT budget
            // (`window*fraction - reserve`), and cutting it starved the reading -- measured,
            // twice, into runs that never answered at all. This caps only the REPLY.
            //
            // The point is to make a ramble cheap. Tiel ignores `/no_think` (a Qwen3
            // directive), so it reasons in prose until something stops it; at 12,288 that
            // costs ~45,000 characters and most of a minute per turn. At 2,048 the same
            // ramble is cut in a few seconds and the repair prompt lands while the run still
            // has budget. The risk documented below -- truncating a long edit payload -- does
            // not exist here: this registry has no edit tool.
            2048.min(cfg.response_reserve_tokens)
        } else {
            cfg.response_reserve_tokens
        };
        strategy.prepare_request(&mut req, registry);
        // FORCE the answer once the model has read enough.
        //
        // On a read-only run every prompt-based nudge has failed: the "call finish now" steer
        // is delivered and ignored, and the read-thrash advice is ignored too -- measured, a
        // grammar run read `bubble.rs@100:140` FOUR times identically and then kept going,
        // burning all 14 steps without ever reaching the file it needed.
        //
        // A grammar is not advice. Narrowing it to `finish` alone means the decoder CANNOT
        // emit another read, so the model must answer with what it has. That is the whole
        // reason to constrain decoding rather than ask nicely.
        if read_only_run && total_reads >= FORCE_FINISH_AFTER {
            if let Some(finish_only) = registry.only(&["finish"]) {
                strategy.prepare_request(&mut req, &finish_only);
            }
        }
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
        peak_reply_tokens = peak_reply_tokens.max(counter.count(&resp.content));
        // What the SERVER did with the prompt we just sent: how much of it it reused
        // from its KV cache versus re-prefilled. A backend that says nothing adds
        // nothing, so an arm running against one stays at 0 rather than reporting a
        // cache miss it never observed.
        total_cached_prompt_tokens += resp.cached_prompt_tokens.unwrap_or(0);
        total_prefilled_prompt_tokens += resp.prefilled_prompt_tokens.unwrap_or(0);
        sink.record(&AgentEvent::ModelTurn {
            step: step + 1,
            prompt_tokens: built.tokens_used,
            cached_prompt_tokens: resp.cached_prompt_tokens,
            prefilled_prompt_tokens: resp.prefilled_prompt_tokens,
            raw: resp.content.clone(),
        });

        // Say so when the reply was OUR fault, not the model's.
        //
        // The comment on `max_tokens` above explains the mechanism; this reports it.
        // A truncated turn usually carries no tool call, and the decode below is about
        // to feed back "no JSON tool object in your reply" -- which reads as a model
        // refusing to act and sent one investigation 54 turns down the wrong path.
        // With this, the transcript says the cap was hit, on the turn it was hit.
        // `finish_reason: "length"` alone is not proof, so require the reply to have
        // ACTUALLY run long.
        //
        // llama.cpp reports `length` when a grammar-constrained decode stops cleanly
        // at the end of a well-formed object, so a complete 20-character
        // `{"tool":"edit_file"}` was being reported as truncated at a 6144-token cap
        // -- with the server's own log saying `truncated = 0`. A detector that fires
        // on a healthy turn is the noise the fault count exists to avoid: it makes
        // "2 harness faults" mean nothing.
        //
        // Half the cap is deliberately generous. A genuine truncation runs to the
        // cap, so it clears this easily; the false positives are all short replies
        // that reached a natural stop.
        //
        // EMPTY content plus `length` is its own certainty, and bypasses `ran_long`. A
        // reasoning model spends its budget in a separate `reasoning_content` field, so a
        // reply cut off mid-thought scores ZERO content tokens and slipped straight through
        // this check -- measured on Tiel, which burned all 700 completion tokens reasoning
        // and returned `content: ""`. The false positive the guard exists for is a
        // grammar-constrained decode stopping at a well-formed object, which by
        // construction HAS content; nothing empty is ever that.
        let ran_long = counter.count(&resp.content) > cfg.response_reserve_tokens / 2;
        let spent_it_all_thinking = resp.content.trim().is_empty();
        // MEASURE the truncation; do not trust the server to report it.
        //
        // `was_truncated()` reads `finish_reason == "length"`, and this backend does not send
        // it for these calls -- verified in the transcript log: three replies of 7,088 /
        // 7,302 / 7,380 chars, every one ending mid-word ("...Unless... the user"), all with
        // `ok=true` and no finish_reason. So every guard keyed on `was_truncated()` was blind
        // exactly when it was needed, and the run recorded ZERO faults while burning turns.
        //
        // The harness knows the cap it asked for, so it can see the reply reach it. 90% is
        // the threshold: a reply that stops one token short of its ceiling has not "finished
        // naturally", and a genuine short answer is nowhere near it.
        let reply_tokens = counter.count(&resp.content);
        let hit_the_cap = req.max_tokens > 0 && reply_tokens * 10 >= req.max_tokens * 9;
        let was_cut_off = resp.was_truncated() || hit_the_cap;
        // Count truncated replies for the WHOLE run, not as a consecutive streak.
        //
        // A streak was wrong: the observed pattern alternates -- the model runs to the cap,
        // gets the repair prompt, emits one good call, then runs to the cap again. A
        // consecutive counter resets on every recovery and never reaches its limit, which is
        // why the previous version of this never fired on a real run even though it fired in
        // the unit test (whose mock truncates every turn).
        //
        // Each truncated reply costs a full prompt pass plus a maximum-length generation --
        // most of a minute on a 35B model -- so a run that keeps hitting the cap is burning
        // the user's time whether or not the waste is contiguous.
        if was_cut_off {
            capped_replies += 1;
        }
        if was_cut_off && (ran_long || spent_it_all_thinking) {
            sink.record(&AgentEvent::HarnessFault {
                kind: FaultKind::ReplyTruncated,
                detail: if spent_it_all_thinking {
                    format!(
                        "the reply stopped at the {}-token cap having emitted NO content at \n                         all -- a reasoning model spent the entire budget thinking, so it \n                         never got to answer. Raise `response_reserve_tokens`.",
                        cfg.response_reserve_tokens
                    )
                } else {
                    format!(
                        "the reply stopped at the {}-token cap after {} chars; any tool call \n                         it was about to emit was cut off. Raise `response_reserve_tokens` \n                         if this repeats, or the model is not converging on a call.",
                        cfg.response_reserve_tokens,
                        resp.content.len()
                    )
                },
                step: step + 1,
            });
        }

        // TWO truncated replies in a row and the run stops.
        //
        // The model is generating to the cap and emitting no tool call. It has already been
        // told what went wrong once, and each further attempt costs a full ~12k-token prompt
        // pass plus a maximum-length generation -- most of a minute on a 35B model -- for
        // nothing. Measured: runs burned 14 steps and 812 seconds this way, with six repair
        // events and ZERO stalls, because the stall detector watches ACTIONS and these turns
        // produce none.
        //
        // Only on a READ-ONLY run: a build run's equivalent (a long `edit_file` old_str that
        // will not encode) has a recovery that works -- the `edit_lines` steer -- and killing
        // it would throw away a run that was about to succeed.
        if read_only_run && capped_replies >= 3 {
            sink.record(&AgentEvent::Stalled {
                trigger: "the model kept generating to the token cap instead of calling a tool"
                    .to_string(),
            });
            let reason = StopReason::Stalled(
                format!("{capped_replies} replies ran to the token cap; the model is not converging on a tool call"),
            );
            sink.record(&AgentEvent::Stopped {
                reason: reason.clone(),
            });
            let (faults, verified) = {
                let log = runlog.lock();
                (log.fault_counts(), log.last_verification_green())
            };
            return Ok(stopped(
                reason,
                step + 1,
                verified,
                &journal,
                metrics,
                peak_prompt_tokens,
                total_prompt_tokens,
                total_cached_prompt_tokens,
                total_prefilled_prompt_tokens,
                peak_reply_tokens,
                faults,
                budget,
                interv.count,
            ));
        }

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
                malformed_streak = 0; // a parseable call broke the malformed-reply streak
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
                    // Junior asks senior (spec 02). Consult the advisor for a nudge, on the
                    // same budget the stall ladder spends.
                    let question = call.str("question").unwrap_or_default();
                    let trigger = format!("ask_user: {question}");
                    match interv.consult(advisor, instruction, &plan, &history, question) {
                        Some(advice) => {
                            stall_detector.reset();
                            sink.record(&AgentEvent::Advice {
                                trigger,
                                advice: advice.clone(),
                            });
                            (advice, action, false, tool, arg)
                        }
                        None => {
                            // No senior (or none left to ask). Ending the run here threw
                            // away everything the model had built up over a question it
                            // could usually answer itself; tell it so and let it decide.
                            interv.count += 1;
                            let advice = String::from(
                                "No one is available to answer. Decide for yourself from \
                                 what you have and continue.",
                            );
                            sink.record(&AgentEvent::Advice {
                                trigger,
                                advice: advice.clone(),
                            });
                            (advice, action, false, tool, arg)
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
                    // Only name a tool the model can actually call (see `mention`).
                    let how = match mention(
                        registry,
                        &["edit_lines", "edit_file", "edit_function", "write_file"],
                    ) {
                        Some("edit_lines") => {
                            "prefer `edit_lines` (give the line numbers shown, no snippet to copy)"
                        }
                        Some("edit_file") => "use `edit_file` with a short, unique anchor",
                        Some("edit_function") => {
                            "use `edit_function` with the function's name and its full new body"
                        }
                        Some(_) => "use `write_file` with the ENTIRE corrected contents",
                        None => "work from the copy shown",
                    };
                    let obs = format!(
                        "`{path}` is ALREADY SHOWN IN FULL above with LINE NUMBERS and updates \
                         after each edit — you do not need to read it. Edit it directly: {how}. \
                         Make your next change now."
                    );
                    report_if_unoffered(&obs, registry, step, sink);
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
                        cfg.external_tool.as_deref(),
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
                                        total_prompt_tokens,
                                        total_cached_prompt_tokens,
                                        total_prefilled_prompt_tokens,
                                        peak_reply_tokens,
                                        harness_faults: runlog.lock().fault_counts(),
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
                            // The harness killed something for running too long.
                            //
                            // Reported as a harness event because the harness is what
                            // noticed and what intervened, even though the cause is
                            // usually the model's: code that compiles and then loops
                            // forever. Without this the intervention is invisible --
                            // the observation just says the command failed, and a run
                            // that lost four minutes to a spinning binary looks the
                            // same as one that got a compile error.
                            if o.contains("[harness] command exceeded its") {
                                sink.record(&AgentEvent::HarnessFault {
                                    kind: FaultKind::CommandTimedOut,
                                    detail: format!(
                                        "`{}` was killed for exceeding its time limit; \
                                         the code it ran probably does not terminate",
                                        arg
                                    ),
                                    step: step + 1,
                                });
                            }
                            if tool == "run_verification" {
                                // Only a *configured* verification with real test
                                // detail counts as green (the "no command" message
                                // isn't a pass).
                                let configured = cfg.verify_command.is_some();
                                // The model asked to run the tests and there were no
                                // tests to run. That records as `green: false`, which
                                // is indistinguishable from a failing suite -- so a
                                // harness that forgot to configure verification scores
                                // as a model that could not make the tests pass. Task
                                // runs sat in exactly this state: `verify_command` was
                                // None, so the agent edited blind and every attempt to
                                // check its own work came back as a failure it caused.
                                if !configured {
                                    sink.record(&AgentEvent::HarnessFault {
                                        kind: FaultKind::VerifyUnavailable,
                                        detail: "the model called `run_verification` but no \
                                                 verify command is configured; its result \
                                                 cannot count as green"
                                            .to_string(),
                                        step: step + 1,
                                    });
                                }
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
                                        total_prompt_tokens,
                                        total_cached_prompt_tokens,
                                        total_prefilled_prompt_tokens,
                                        peak_reply_tokens,
                                        harness_faults: runlog.lock().fault_counts(),
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
                                let note = match mention(registry, &["append_file"]) {
                                    Some(append) => format!(
                                        "{o}\nNOTE: your reply was cut off, so only the part above \
                                         was saved. Do NOT re-send the whole file — continue it \
                                         with `{append}` (same path), adding the NEXT chunk only. \
                                         Repeat `{append}` until the file is complete."
                                    ),
                                    None => format!(
                                        "{o}\nNOTE: your reply was cut off, so only the part above \
                                         was saved. Write the file again in SMALLER pieces: keep \
                                         each reply well under the length that was cut."
                                    ),
                                };
                                report_if_unoffered(&note, registry, step, sink);
                                note
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
                // A TRUNCATED reply is not a malformed one, and must not count toward the
                // malformed strike limit.
                //
                // Both were being counted together, so one ramble (strike 1) plus a single
                // further bad parse (strike 2) ended the run at exactly the moment the
                // truncation steer would have taken effect -- measured, a run died on ONE
                // JSON error because a ramble had already used the other strike. Truncation
                // has its own budget (`capped_replies`) and its own steer; this counter is
                // for replies that are genuinely unparseable.
                if !was_cut_off {
                    malformed_streak += 1;
                }
                let mut detail = e.repair_prompt_for(registry);
                // A read-only run that just ran to the cap was THINKING, not malformed.
                //
                // The generic repair prompt explains JSON syntax, which is useless advice for
                // a model whose problem is that it never stopped reasoning. Observed: it came
                // back with "I got confused in my previous reasoning. Let me re-read the code
                // carefully" -- and re-read, and was cut off again.
                //
                // Tell it the one thing that ends the loop: it already has the code, and the
                // answer goes in `finish`.
                if read_only_run && was_cut_off {
                    detail.push_str(
                        // ONE LINE: a backslash-continued literal gets reflowed by
                        // rustfmt into literal spaces, which happened to this very
                        // string once already.
                        "\n\nYour reply was cut off because it ran too long. You are THINKING OUT LOUD instead of answering. Do not re-read anything - the code you need is already above. Reply with ONE JSON object now: {\"tool\":\"finish\",\"summary\":\"<your answer: the file, the line, the cause, the fix>\"}. Keep it under 200 words.",
                    );
                } else if was_cut_off {
                    // A BUILD run that ran to the cap without emitting a call was reasoning,
                    // not malforming -- and until now it got only the generic JSON-syntax
                    // repair prompt, which is advice for a problem it does not have.
                    //
                    // Measured on the 126-run ladder: 38 replies ran to the 2048-token cap
                    // and consumed 787 of 3,705 seconds -- 21% of the wall-clock for 5% of
                    // the calls. Their text is unmistakable and self-aware: "I keep planning
                    // without acting. Let me just write the implementation", followed by
                    // another 2,000 tokens of planning. The model knew; nothing was telling
                    // it to stop.
                    //
                    // The cap itself was raised past the distribution's shoulder (see
                    // `response_reserve_tokens`), so this is the backstop for the turns that
                    // still run long, not the primary fix.
                    let how = match mention(registry, &["write_file", "edit_file", "create_file"]) {
                        Some(t) => format!("call `{t}` with the change"),
                        None => "make the change".to_string(),
                    };
                    detail.push_str(&format!(
                        // ONE LINE, for the rustfmt-reflow reason given just above.
                        "\n\nYour reply was cut off: it ran to the token limit without emitting a tool call, so this turn did nothing. You are planning instead of acting. Stop reasoning and {how} NOW, in ONE JSON object, as your very first output - no preamble, no explanation. If the change is large, make the smallest correct part of it this turn."
                    ));
                }
                // Repeated malformed replies usually mean the model is trying to encode a long
                // multi-line `edit_file` `old_str` as JSON and mangling it. Steer to `edit_lines`
                // (line numbers, no old_str) so the encoding problem disappears.
                if malformed_streak >= 2 {
                    // Again, only tools the registry actually has -- see `mention`.
                    let steer = match (
                        mention(registry, &["edit_lines"]),
                        mention(registry, &["edit_file"]),
                    ) {
                        (Some(_), Some(_)) => {
                            "\n\nYou have produced an unparseable reply more than once — this usually \
                             happens when `edit_file`'s `old_str` is a long multi-line snippet that is \
                             hard to encode as JSON. STOP using edit_file for this. Use `edit_lines` \
                             instead: {\"tool\":\"edit_lines\",\"path\":\"<file>\",\"start\":<n>,\
                             \"end\":<m>,\"new_text\":\"<the replacement>\"}. It takes the LINE NUMBERS \
                             shown in the file view (no snippet to copy), so the reply stays short and \
                             valid."
                        }
                        (None, Some(_)) => {
                            "\n\nYou have produced an unparseable reply more than once — this usually \
                             happens when `edit_file`'s `old_str` is a long multi-line snippet that is \
                             hard to encode as JSON. Keep `old_str` SHORT: one or two distinct lines \
                             are enough to anchor on, and the reply stays valid."
                        }
                        (Some(_), None) => {
                            "\n\nYou have produced an unparseable reply more than once — this usually \
                             happens when a long multi-line snippet is hard to encode as JSON. Use \
                             `edit_lines`: {\"tool\":\"edit_lines\",\"path\":\"<file>\",\"start\":<n>,\
                             \"end\":<m>,\"new_text\":\"<the replacement>\"}. It takes LINE NUMBERS, \
                             so the reply stays short and valid."
                        }
                        (None, None) => {
                            "\n\nYou have produced an unparseable reply more than once. Keep the \
                             reply to ONE short JSON object: no prose around it, and no long \
                             multi-line strings inside it."
                        }
                    };
                    detail.push_str(steer);
                    report_if_unoffered(steer, registry, step, sink);
                    interv.count += 1;
                }
                sink.record(&AgentEvent::RepairTriggered {
                    detail: first_line(&detail),
                });
                // TWO unparseable replies in a row and the run stops.
                //
                // The stall detector never sees this: it observes ACTIONS, and a reply that
                // fails extraction produces none. Measured on a read-only run, the model
                // stopped emitting tool calls once it had read enough and wrote 30,000-
                // character explanations on eight consecutive turns -- six RepairTriggered
                // events, ZERO Stalled events, ending `BudgetExhausted` 812 seconds later
                // with nothing returned. Every one of those turns was a full prompt resend.
                //
                // A model that could not produce a call twice, having just been told exactly
                // how, is not going to produce one on the ninth attempt. Stopping returns
                // the same (empty) result far sooner and leaves a stop reason that says why.
                // Only on a READ-ONLY run, and NOT when the reply was truncated.
                //
                // A truncated read-only reply now gets its own steer above ("you are thinking
                // out loud; call finish"), and that steer needs a turn to land. This guard
                // used to fire first and end the run before the model ever saw it -- measured,
                // the steer text never reached the prompt at all. Truncation is handled by
                // `capped_replies`; this one is for a reply that is genuinely unparseable.
                // 3, not 2. The steer needs turns to work.
                //
                // Measured across four runs of the user's own question: at 2 the run died
                // while the model was still converging -- it had read the right function and
                // was one turn from answering. The extra turn costs seconds; ending early
                // costs the whole run, and the user sees "did not reach a conclusion" after
                // waiting a minute.
                if read_only_run && !was_cut_off && malformed_streak >= 2 {
                    sink.record(&AgentEvent::Stalled {
                        trigger: "two unparseable replies in a row".to_string(),
                    });
                    let reason =
                        StopReason::Stalled("two unparseable replies in a row".to_string());
                    sink.record(&AgentEvent::Stopped {
                        reason: reason.clone(),
                    });
                    let (faults, verified) = {
                        let log = runlog.lock();
                        (log.fault_counts(), log.last_verification_green())
                    };
                    return Ok(stopped(
                        reason,
                        step + 1,
                        verified,
                        &journal,
                        metrics,
                        peak_prompt_tokens,
                        total_prompt_tokens,
                        total_cached_prompt_tokens,
                        total_prefilled_prompt_tokens,
                        peak_reply_tokens,
                        faults,
                        budget,
                        interv.count,
                    ));
                }
                (
                    detail,
                    action_hash("(malformed)", ""),
                    false,
                    "(malformed)".to_string(),
                    String::new(),
                )
            }
        };

        // edit_file anchor-loop breaker (spec 03): a non-matching `edit_file` (the
        // anchor isn't in the file) is a mutating call that errored, yet a small model
        // will re-submit the same imagined anchor until the stall kills it. Track repeated
        // misses on the same path and, after a couple, steer it to rewrite the whole
        // file instead of hunting for an anchor that doesn't exist.
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
            // Every tool named below comes from `mention`: whichever way the model was
            // failing, the way out has to be a tool it can actually call.
            let whole = mention(registry, &["write_file"]);
            let surgical = mention(registry, &["edit_function", "edit_lines"]);
            let directive = match (create_clash && !big_existing, big_existing, whole, surgical) {
                (true, _, Some(w), _) => format!(
                    "`{arg}` already exists — `create_file` will NOT overwrite it, so \
                     repeating it does nothing. To change it, call `{w}` with `path` \
                     `{arg}` and the ENTIRE new file contents in one shot ({w} \
                     overwrites). Make the fix the failing test needs."
                ),
                (_, true, _, Some("edit_function")) => format!(
                    "Editing `{arg}` by exact snippet is failing — you keep matching code that \
                     isn't in the file. STOP editing this large file by snippet. If the code you \
                     want to change is inside a function/method, use `edit_function`: pass its \
                     `name` and the FULL new function text as `new_body` — no snippet to copy and \
                     no line numbers to get right (the tool finds the function for you). This is \
                     the easiest way to add a match arm or change a body.{} Make the change now.",
                    if registry.get("edit_lines").is_some() {
                        " Otherwise use `edit_lines` (address lines by NUMBER from the `N: ` view; \
                         do NOT include the `N: ` prefix; to INSERT before line N pass start=N, \
                         end=N-1)."
                    } else {
                        ""
                    }
                ),
                (_, true, _, Some(_)) => format!(
                    "Editing `{arg}` by exact snippet is failing — you keep matching code that \
                     isn't in the file. STOP editing this large file by snippet. Use `edit_lines` \
                     (address lines by NUMBER from the `N: ` view; do NOT include the `N: ` \
                     prefix; to INSERT before line N pass start=N, end=N-1). Make the change now."
                ),
                (_, false, Some(w), _) => format!(
                    "Your edit anchor does not exist in `{arg}` — you are matching against \
                     code that isn't in the file. STOP editing by anchor. Instead call `{w}` \
                     with `path` `{arg}` and the ENTIRE corrected file contents in one shot \
                     ({w} overwrites the existing file). Base it on the file shown in the error \
                     above plus the fix the failing test needs."
                ),
                (_, _, None, Some(s)) => format!(
                    "Repeating that call on `{arg}` does nothing — you are working from code \
                     that isn't in the file. STOP guessing at its contents: use `{s}` on the \
                     file as it is actually shown above, and make the fix the failing test needs."
                ),
                // A big file with no surgical tool, or nothing that writes at all: the only
                // honest advice is to stop imagining the file and work from the shown copy.
                _ => format!(
                    "Repeating that call on `{arg}` does nothing — you are working from code \
                     that isn't in the file. Re-read the file as it is actually shown above and \
                     copy the anchor from it exactly."
                ),
            };
            report_if_unoffered(&directive, registry, step, sink);
            sink.record(&AgentEvent::Advice {
                trigger: if create_clash {
                    "create_file keeps clashing with an existing file".to_string()
                } else {
                    "edit_file anchor keeps missing".to_string()
                },
                advice: directive.clone(),
            });
            // Appended to the tool's real error, never in place of it: the error names the
            // anchor that missed (or the file that exists), and that is the evidence the
            // model needs to follow the directive.
            format!("{obs}\n\n{directive}")
        } else {
            obs
        };

        // Reads are counted, not judged. Re-reading is the stall detector's business
        // (a non-novel action since the last change is non-progress, and it trips
        // `Stuck`); the count only feeds the read-only force-finish above.
        if matches!(
            tool.as_str(),
            "read_file" | "read_function" | "search_code" | "list_dir"
        ) {
            total_reads += 1;
        }

        // Record the turn and detect stalls (spec 03 — VERIFY, cheap every turn).
        let was_error = looks_like_failure(&obs);
        sink.record(&AgentEvent::ToolResult {
            summary: first_line(&obs),
            full: obs.clone(),
            is_error: was_error,
        });
        history.push(TurnRecord::new(tool.clone(), arg, was_error));
        // Two different shapes of output need two different cuts, and the cap
        // (`observation_cap_for`) is the same source of truth for both.
        //
        // A paged file read is CONTIGUOUS source the model is about to edit, and it can
        // always ask for the next page — so it is cut to a contiguous prefix that names
        // the start which resumes it. The head/tail slice used to drop the MIDDLE of a
        // big read while the header still claimed the full range: a model that asked for
        // 2800-5800 got 2800-3199 and 5401-5800 and a hole where the code it wanted was,
        // with no way to name the missing region. That was the reported "cannot read
        // around line 2800" on an 8,000-line file.
        //
        // Everything else — a verification report, a shell log, an `ask` answer — keeps
        // the error-first path: those genuinely want the failing lines over the leading
        // ones, and their lines are not a range the model can re-request.
        let cap = observation_cap_for(&tool, cfg);
        let trimmed = if matches!(tool.as_str(), "read_file" | "read_function") {
            truncate_paged_read(&obs, cap)
        } else {
            truncate_observation(&obs, cap, true)
        };
        // Did the cap just hide the answer? Capping is usually right -- error-first
        // truncation keeps the failing lines, and a paged read keeps a contiguous prefix
        // and names the `start` that fetches the rest. The case worth a fault is the
        // BLIND cut: no error line to anchor on, so a head/tail slice dropped the middle
        // unseen, and it dropped more than it kept. The model then decides on less than
        // half the evidence and its next call looks like it ignored the output. A cut
        // paged read is NOT that -- it is a page boundary the model can turn -- and
        // `blind_cut` keys on a marker only the head/tail slice writes, so it stays
        // quiet for one however much it left behind.
        if let Some((total, shown)) = blind_cut(&obs, &trimmed) {
            sink.record(&AgentEvent::HarnessFault {
                kind: FaultKind::ObservationTruncated,
                detail: format!(
                    "`{tool}` returned {total} lines; the cap showed the model {shown} and \
                     dropped {} from the middle with no error line to anchor on. Raise \
                     `observation_line_cap` if the model then acts on what it did not see.",
                    total - shown
                ),
                step: step + 1,
            });
        }
        recent.push_turn(&resp.content, &trimmed);
        // Re-render the cached retrieval only if this turn changed the workspace, and then
        // only the parts whose bytes moved; an unchanged turn keeps the prompt prefix intact.
        // `changed` only tracks the path-carrying edit tools, but a shell command can edit
        // anything (`sed -i`, a build that generates a file), so a `run_command` turn is
        // treated as a possible change: the refresh compares content hashes, so a command
        // that changed nothing costs a few reads and re-renders nothing.
        stable.refresh_if_changed(workspace, cfg, registry, changed || tool == "run_command");

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
                // Uncapped: the parser must see the whole suite, or a long run loses its
                // FAILED lines to the 16 KB command cap. The model only ever sees the
                // parsed, failure-first report, so the cap is not needed here.
                let cmd_result = sc_verify::run_command_full(&cfg.sandbox, workspace, cmd);
                let mut report = sc_verify::parse(cmd, &cmd_result.output, cmd_result.ok);
                // Same delta the model-invoked run_verification gets: "same N failures as
                // last run" is the signal that an edit did nothing.
                report.delta = sc_verify::note_run(cmd, &report);
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
                        total_prompt_tokens,
                        total_cached_prompt_tokens,
                        total_prefilled_prompt_tokens,
                        peak_reply_tokens,
                        harness_faults: runlog.lock().fault_counts(),
                        prompt_budget: budget,
                        verified: Some(true),
                        change_summary: journal.change_summary(),
                        stop_reason: StopReason::Finished,
                        interventions: interv.count,
                    });
                } else {
                    let observation = report.observation();
                    let sig = failure_signature(&observation);
                    if last_failure_sig == Some(sig) {
                        failure_sig_streak += 1;
                    } else {
                        last_failure_sig = Some(sig);
                        failure_sig_streak = 1;
                    }
                    if failure_sig_streak >= UNCHANGED_FAILURE_LIMIT {
                        stall_detector.note_unchanged_failure();
                    }
                    // Surface the failing tests so the next turn is grounded.
                    let fb = format!("(harness ran the tests after your edit)\n{observation}");
                    // Use the generous read_file cap, not the tight log cap: the report
                    // is failure-first and carries the underlying exception (e.g.
                    // TemplateNotFound) that the model must see to fix the bug. At the
                    // 40-line log cap the `✗`/assert headers crowded the real exception
                    // out, so the model only saw a bare `assert ... == ...` (observed
                    // live) and looped blind. 400 lines still bounds a degenerate suite.
                    recent.push_observation(&truncate_observation(
                        &fb,
                        cfg.read_file_line_cap,
                        true,
                    ));
                }
            }
        }

        match stall::handle_stall(
            step,
            action,
            changed,
            registry,
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
                let (faults, verified) = {
                    let log = runlog.lock();
                    (log.fault_counts(), log.last_verification_green())
                };
                return Ok(stopped(
                    reason,
                    step + 1,
                    verified,
                    &journal,
                    metrics,
                    peak_prompt_tokens,
                    total_prompt_tokens,
                    total_cached_prompt_tokens,
                    total_prefilled_prompt_tokens,
                    peak_reply_tokens,
                    faults,
                    budget,
                    interv.count,
                ));
            }
        }
    }

    sink.record(&AgentEvent::Stopped {
        reason: StopReason::BudgetExhausted,
    });
    // Bound before the call: taking the lock inline makes the guard a temporary of
    // the tail expression, which outlives `runlog` itself.
    let (faults, verified) = {
        let log = runlog.lock();
        (log.fault_counts(), log.last_verification_green())
    };
    Ok(stopped(
        StopReason::BudgetExhausted,
        cfg.max_steps,
        verified,
        &journal,
        metrics,
        peak_prompt_tokens,
        total_prompt_tokens,
        total_cached_prompt_tokens,
        total_prefilled_prompt_tokens,
        peak_reply_tokens,
        faults,
        budget,
        interv.count,
    ))
}

mod assemble;
mod config;
mod dispatch;
mod escalation;
mod prompt;
mod stable;
mod stall;
mod window;

#[cfg(test)]
mod test_util;
#[cfg(test)]
mod tests;

use dispatch::{
    blind_cut, dispatch, gate_finish, key_arg, looks_like_failure, mutating_path,
    observation_cap_for, pre_apply_batched_writes, FinishGate,
};
use escalation::{mention, stopped};
use stable::StableContext;
use window::{role_word, RecentWindow};
