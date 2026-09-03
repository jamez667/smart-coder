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

use sc_context::{prompt_budget, truncate_observation, ContextBuilder, TokenCounter, TurnRecord};
use sc_index::Boosts;
use sc_model::{GenerateRequest, Message, ModelBackend};
use sc_proto::Result;
use sc_tools::{Journal, ToolOutcome, ToolRegistry};

pub use config::{AgentConfig, AgentReport};
use config::{FOCUS_TASK_PREFIX, INVESTIGATE_TASK_PREFIX, TASK_PREFIX, TASK_PREFIX_SHELL};

use crate::event::{AgentEvent, EventSink, FaultKind, NullSink};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::planner::make_plan;
use crate::recovery::{action_hash, StallDetector, StopReason};
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
    // Every tool the harness knows how to build, taken from the DEFAULT registry
    // rather than a hand-kept list.
    //
    // The list used to be hardcoded here and had drifted: it was missing
    // `search_code`, `read_function`, `edit_function`, `find_symbol`,
    // `append_file` and `create_file`, so guidance naming any of those against a
    // trimmed registry was invisible to this detector -- which is exactly what it
    // exists to catch. Deriving it means a new tool is covered the day it is added.
    let known = sc_tools::default_registry();
    let known: Vec<&str> = known.specs().iter().map(|s| s.name).collect();

    for m in messages {
        // Only harness-authored text. A user instruction naming a tool is not our bug.
        if m.role == sc_model::Role::User {
            continue;
        }
        for name in &known {
            if registry.get(name).is_some() {
                continue;
            }
            if m.content.contains(&format!("`{name}`")) {
                return Some((*name).to_string());
            }
        }
    }
    None
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
    let prefix = if read_only_run {
        INVESTIGATE_TASK_PREFIX
    } else if !cfg.focus_files.is_empty() {
        FOCUS_TASK_PREFIX
    } else if cfg.permission.allow_shell {
        TASK_PREFIX_SHELL
    } else {
        TASK_PREFIX
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
    // Largest reply seen, so the reply reserve can be checked against reality.
    let mut peak_reply_tokens = 0usize;
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
    // Consecutive turns whose reply didn't parse into a tool call at all (malformed JSON). A
    // coder model encoding a long multi-line `edit_file` `old_str` often produces JSON the
    // parser can't extract, and re-tries the SAME malformed call — the anchor-miss breaker never
    // fires (the call never parsed). After a couple, steer it to `edit_lines`, which takes line
    // NUMBERS and no big `old_str`, sidestepping the encoding problem (observed live 2026-07-15).
    let mut malformed_streak = 0usize;
    // Read-thrash guard: how many read_file calls have happened since the last workspace change.
    // A small model often re-reads the same files many times before acting (observed live on
    // void-claim: schema.rs read 6+ times in one run, burning budget). Paging through a large
    // file is legitimate, so we don't block reads — but past a threshold with NO edit, we inject
    // a firm "you have enough; act now" nudge and reset. Cleared on any change.
    let mut reads_since_change = 0usize;
    // How many times the read-thrash nudge has fired since the last edit.
    //
    // The nudge alone is not enough. It resets its own counter when it fires, so it
    // warns every N reads forever and never escalates -- measured on a real run:
    // 27 turns, four files cycled, ZERO edits, the "do not read again" nudge
    // delivered twice and ignored both times. Past a second warning the harness
    // stops asking and starts refusing the read outright.
    let mut read_nudges_since_change = 0usize;
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
                peak_reply_tokens,
                harness_faults: runlog.lock().fault_counts(),
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
            registry,
        );

        let built = builder.build(segments);
        peak_prompt_tokens = peak_prompt_tokens.max(built.tokens_used);

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
        req.max_tokens = cfg.response_reserve_tokens;
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
        peak_reply_tokens = peak_reply_tokens.max(counter.count(&resp.content));
        sink.record(&AgentEvent::ModelTurn {
            step: step + 1,
            prompt_tokens: built.tokens_used,
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
        if resp.was_truncated() && (ran_long || spent_it_all_thinking) {
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
                                peak_reply_tokens,
                                runlog.lock().fault_counts(),
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
                    // Only name a tool the model can actually call. A trimmed registry
                    // (spec 04/08 — fewer choices, more action) may not carry
                    // `edit_lines`, and steering to a tool that is not offered wastes
                    // the turn and teaches the model to distrust the harness.
                    let how = if registry.get("edit_lines").is_some() {
                        "prefer `edit_lines` (give the line numbers shown, no snippet to copy)"
                    } else {
                        "use `edit_file` with a short, unique anchor"
                    };
                    let obs = format!(
                        "`{path}` is ALREADY SHOWN IN FULL above with LINE NUMBERS and updates \
                         after each edit — you do not need to read it. Edit it directly: {how}. \
                         Make your next change now."
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
                malformed_streak += 1;
                let mut detail = e.repair_prompt();
                // Repeated malformed replies usually mean the model is trying to encode a long
                // multi-line `edit_file` `old_str` as JSON and mangling it. Steer to `edit_lines`
                // (line numbers, no old_str) so the encoding problem disappears.
                if malformed_streak >= 2 {
                    // Again, only if the registry actually has it — see the
                    // read-redirect above.
                    detail.push_str(if registry.get("edit_lines").is_some() {
                        "\n\nYou have produced an unparseable reply more than once — this usually \
                         happens when `edit_file`'s `old_str` is a long multi-line snippet that is \
                         hard to encode as JSON. STOP using edit_file for this. Use `edit_lines` \
                         instead: {\"tool\":\"edit_lines\",\"path\":\"<file>\",\"start\":<n>,\
                         \"end\":<m>,\"new_text\":\"<the replacement>\"}. It takes the LINE NUMBERS \
                         shown in the file view (no snippet to copy), so the reply stays short and \
                         valid."
                    } else {
                        "\n\nYou have produced an unparseable reply more than once — this usually \
                         happens when `edit_file`'s `old_str` is a long multi-line snippet that is \
                         hard to encode as JSON. Keep `old_str` SHORT: one or two distinct lines \
                         are enough to anchor on, and the reply stays valid."
                    });
                    interv.count += 1;
                }
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
                    "Editing `{arg}` by exact snippet is failing — you keep matching code that \
                     isn't in the file. STOP using edit_file on this large file. If the code you \
                     want to change is inside a function/method, use `edit_function`: pass its \
                     `name` and the FULL new function text as `new_body` — no snippet to copy and \
                     no line numbers to get right (the tool finds the function for you). This is \
                     the easiest way to add a match arm or change a body. Otherwise use \
                     `edit_lines` (address lines by NUMBER from the `N| ` view; do NOT include the \
                     `N| ` prefix; to INSERT before line N pass start=N, end=N-1). Make the change \
                     now with edit_function or edit_lines."
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

        // Read-thrash guard: count reads since the last change; on a change, reset. Past the
        // threshold with no edit, append a firm "you have enough — act now" nudge so the model
        // stops re-reading files it already has in context and makes a concrete edit. Paging a
        // large file is fine up to the threshold; this only fires when reads pile up WITHOUT any
        // workspace change (the observed void-claim thrash: schema.rs read 6+ times, no edit).
        const READ_THRASH_LIMIT: usize = 5;
        if changed {
            reads_since_change = 0;
            // DECAY the escalation rather than clearing it. A full reset means one
            // edit buys a whole fresh read budget, so a model can alternate
            // 5-reads-then-one-edit indefinitely and never be refused -- measured on
            // the run this was found in: 25 reads against 2 edits, 12:1, with the
            // refusal firing and then being reset away twice. Decaying keeps the
            // pressure on a model that is mostly reading while still rewarding an
            // edit, and a genuinely productive run drops back to zero within a
            // couple of edits.
            read_nudges_since_change = read_nudges_since_change.saturating_sub(1);
        } else if matches!(
            tool.as_str(),
            "read_file" | "read_function" | "search_code" | "list_dir"
        ) {
            reads_since_change += 1;
        }
        let obs = if reads_since_change >= READ_THRASH_LIMIT {
            reads_since_change = 0;
            read_nudges_since_change += 1;
            interv.count += 1;
            // Asking twice is generous; a third round means the words are not
            // working and repeating them just burns the budget.
            if read_nudges_since_change >= 2 {
                format!(
                    "{obs}\n\n[harness] STOP READING. This is the second time you have been told \
                     you already have what you need. Make one concrete edit THIS turn."
                )
            } else {
                // Names no specific edit tool: the registry may be trimmed, and
                // steering toward one the model was not given wastes the turn. This
                // used to name `edit_function`, which a six-tool run cannot call.
                format!(
                    "{obs}\n\n[harness] You have read {READ_THRASH_LIMIT}+ times without changing any \
                     file. You already have what you read in the context above — re-reading won't \
                     help. Make a CONCRETE edit THIS turn with one of the edit tools you were \
                     given, then run the tests."
                )
            }
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
                let cmd_result = sc_verify::run_command_in(&cfg.sandbox, workspace, cmd);
                let report = sc_verify::parse(cmd, &cmd_result.output, cmd_result.ok);
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
                        peak_reply_tokens,
                        harness_faults: runlog.lock().fault_counts(),
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
                    peak_reply_tokens,
                    runlog.lock().fault_counts(),
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
    let faults = runlog.lock().fault_counts();
    Ok(stopped(
        StopReason::BudgetExhausted,
        cfg.max_steps,
        &cfg.sandbox,
        &cfg.verify_command,
        workspace,
        &journal,
        metrics,
        peak_prompt_tokens,
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
