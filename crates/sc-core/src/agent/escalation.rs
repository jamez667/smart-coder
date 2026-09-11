//! Stall-recovery helpers: the advisor-free self-recovery directive a single model gets
//! when it loops, the advisor consult ("junior asks senior"), and the non-finished stop
//! report the loop returns when it gives up.

use sc_context::{summarize_history, TurnRecord};
use sc_model::ModelBackend;
use sc_tools::{Journal, ToolRegistry};

use crate::advisor::{advice_observation, consult, Predicament};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::recovery::StopReason;

use super::AgentReport;

/// How many times the harness self-recovers from a stall WITHOUT an advisor before giving up.
pub(super) const SELF_RECOVERY_LIMIT: usize = 2;

/// How many times the advisor (the senior T1 model) may be consulted in one run.
///
/// Bounded like its two sibling rungs, and for the same reason: each call costs a
/// full generation on the expensive model. Left unbounded, a stalling run consulted
/// the senior on every stall until the step cap.
pub(super) const ADVISOR_LIMIT: usize = 3;

/// How many root-cause diagnoses the harness runs per run before falling through to the
/// generic recovery ladder. Bounded like [`SELF_RECOVERY_LIMIT`]: each costs a suite run +
/// model call, and a model that ignored two pointed diagnoses won't be saved by a third.
pub(super) const DIAGNOSIS_LIMIT: usize = 2;

/// The last few distinct tools the model has used, most-recent first — context for
/// the self-recovery directive so it names what the model keeps doing.
/// The path the most recent MUTATING turn was aimed at, if any.
///
/// The stall ladder needs it to answer one question before it recommends a wholesale
/// rewrite: is this file too large for `write_file` to accept? `recent_tools` throws the
/// arg away, and without it the directive can only guess -- see
/// [`self_recovery_directive`]'s `oversize_target`.
///
/// Only mutating tools count. A `read_file` arg is a path too, but the file the model last
/// READ is not necessarily the file it is failing to WRITE, and steering off the wrong one
/// is how this class of bug started.
pub(super) fn recent_edit_path(history: &[TurnRecord]) -> Option<&str> {
    history
        .iter()
        .rev()
        .find(|t| {
            matches!(
                t.tool.as_str(),
                "edit_file" | "edit_lines" | "edit_function" | "write_file" | "create_file"
            ) && !t.arg.is_empty()
        })
        .map(|t| t.arg.as_str())
}

pub(super) fn recent_tools(history: &[TurnRecord]) -> Vec<String> {
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

/// The first of `preferred` that this run's registry actually offers.
///
/// The only way a harness directive may put a tool name in front of the model. A
/// trimmed registry (spec 04/08 -- fewer choices, more action) may not carry
/// `edit_lines` or `write_file`, and a model does what it is told: steering it toward a
/// tool it cannot call wastes the turn and teaches it to distrust the harness. Callers
/// list their preferences best-first and word the directive around whichever survives;
/// `None` means say nothing tool-specific.
pub(super) fn mention(registry: &ToolRegistry, preferred: &[&'static str]) -> Option<&'static str> {
    preferred
        .iter()
        .copied()
        .find(|name| registry.get(name).is_some())
}

/// A firm, advisor-free recovery instruction injected when a single model stalls.
/// It names the loop and gives the model a concrete decision: if you've read what
/// you need, EDIT now; if the suite is the blocker, fix the failure it reported. The
/// model has no senior to ask, so the harness has to be the one that breaks the loop.
///
/// Every tool it names comes from [`mention`], so a trimmed registry gets a directive
/// built from the tools it has -- and a read-only one is told to answer, not to edit.
///
/// THE ONE INVARIANT: the directive never recommends and forbids the same tool.
///
/// It used to. The recommended actions were built from `[whole, anchored]` unfiltered, so a
/// model looping on `edit_file` -- the common case, because the edit tools are the ones a
/// model gets stuck on -- was handed "Emit `write_file` or `edit_file` … Do NOT emit
/// `edit_file` again" in a single sentence. Measured over one Mellum run across four rungs,
/// this fired 8 times: 3 of the next turns were `run_verification`/`run_command`/`read_file`
/// (the do-nothing turns the nudge exists to prevent), 2 were `write_file` (the intent) and
/// 3 were `edit_file` (the prohibition ignored). The prohibition is the clearer half of a
/// contradiction, so the model obeyed it and dropped the recommendation more often than not.
///
/// So `looped` is excluded from everything the directive recommends, and the prohibition is
/// only appended when the directive did not just recommend that tool. When excluding it
/// leaves nothing -- the model is looping on the only edit tool the registry offers --
/// "do not use it again" is simply wrong advice, and the directive says how to use that tool
/// DIFFERENTLY instead. Either way the model is always left at least one legal concrete move.
///
/// `evicted` is how many whole turns have left the recent window into the compacted summary.
/// While it is zero the model really does still have everything it read, and the directive
/// says so; once turns have been evicted that claim is false, and a harness that asserts
/// something the model can see is false teaches it to distrust the rest of the directive.
/// `oversize_target` says the file this loop is about is larger than
/// [`sc_tools::WRITE_FILE_OVERWRITE_MAX_LINES`], so `write_file` WILL refuse to overwrite it.
///
/// THE DEADLOCK THIS ENDS. `write.rs`'s own doc comment on that constant predicted it: *"the
/// agent loop's failed-edit escalation has to answer the SAME question before it steers a
/// stuck model at `write_file`: telling it to rewrite a file this guard will then refuse is a
/// deadlock."* The failed-edit path in `mod.rs` does ask (via `rewrite_target`). This one
/// never did, because it takes no workspace and no path.
///
/// Measured on `engine-ecs-query`, run 3, steps 27-28 -- one turn apart:
///
/// ```text
///   advice: Emit `write_file` … this turn. Do NOT emit `edit_file` again.
///   result: write_file src/world.rs rejected: … 276 lines — too large to safely
///           overwrite … Use edit_file
/// ```
///
/// The harness recommended `write_file`, forbade `edit_file`, and then refused `write_file`
/// and told it to use `edit_file`. Both moves closed in two turns.
///
/// With the flag set, `write_file` is dropped from the recommendations. On a six-tool run
/// where the model is looping on `edit_file` that empties both lists, which routes into the
/// `only_edit_tool` branch -- the branch already written for "you are looping on the only
/// tool that can change the workspace", whose advice (widen or re-copy the anchor EXACTLY) is
/// the correct guidance here and never forbids the one tool that can still work.
pub(super) fn self_recovery_directive(
    recent: &[String],
    registry: &ToolRegistry,
    evicted: usize,
    oversize_target: bool,
) -> String {
    let looped = recent
        .first()
        .map(String::as_str)
        .unwrap_or("the same tool");
    // The recommendable edit tools, with the looped one removed: it must never appear as a
    // recommendation next to its own prohibition.
    let keep = |t: Option<&'static str>| t.filter(|n| *n != looped);
    // A file `write_file` will refuse is not a recommendable whole-rewrite target, however
    // much the registry offers the tool. Dropping it here (rather than wording around it
    // later) is what routes an `edit_file` loop on a big file into `only_edit_tool`.
    let whole = if oversize_target {
        None
    } else {
        keep(mention(registry, &["write_file", "create_file"]))
    };
    let anchored = keep(mention(
        registry,
        &["edit_file", "edit_lines", "edit_function"],
    ));
    let verify = keep(mention(registry, &["run_verification"]));
    // Does the registry offer an edit tool at all? Distinct from `whole`/`anchored` being
    // Some: a single-edit-tool registry the model is looping on leaves both None here.
    let only_edit_tool = whole.is_none()
        && anchored.is_none()
        && mention(
            registry,
            &[
                "write_file",
                "create_file",
                "edit_file",
                "edit_lines",
                "edit_function",
            ],
        )
        .is_some();

    let have = if evicted == 0 {
        "You already have everything you read in the context above; re-reading or \
         re-running changes nothing."
    } else {
        // Older turns are gone into the summary, so "you have everything" would be a lie.
        // What is still true is the part that matters: repeating the call will not help.
        "Older turns have been compacted into the summary above, but repeating `{looped}` \
         will not bring them back -- it returns what it returned before."
    };
    let have = have.replace("{looped}", looped);
    let mut out = format!(
        "STOP — you are stuck in a loop calling `{looped}` and making no progress. \
         {have} Decide the next CONCRETE move right now:\n"
    );

    if only_edit_tool {
        // The model is looping on the ONLY tool that can change the workspace. Telling it
        // to stop using that tool would leave it no legal move at all, so tell it how to
        // use the tool differently instead -- the loop is nearly always the same call with
        // the same arguments failing the same way.
        out.push_str(&format!(
            "`{looped}` is the only tool here that can change the workspace, so keep using \
             it -- but NOT with the same arguments, which is what has failed every turn so \
             far. Change ONE of these and send it again:\n\
             - If the anchor/old text did not match, widen or re-copy it EXACTLY from the \
             file as it reads now, whitespace included.\n\
             - If you cannot find an anchor you trust, send the ENTIRE corrected file \
             contents in one shot instead of a fragment.\n\
             - If you are editing blind, open the file first and copy the target lines out \
             of what you get back.\n"
        ));
        out.push_str(&format!(
            "Emit `{looped}` this turn with DIFFERENT arguments."
        ));
        return out;
    }

    match (whole, anchored) {
        (None, None) => {
            // Nothing here can change the workspace: this is a question, and the only
            // move left is to answer it.
            let finish = mention(registry, &["finish"]).unwrap_or("finish");
            out.push_str(&format!(
                "- If you have the answer, call `{finish}` NOW with it in `summary`.\n\
                 - If you are reading the wrong file, say so in `{finish}` rather than \
                 reading on.\n"
            ));
            // Only forbid the looped tool if we did not just recommend it. A model looping
            // on `finish` is being told to call `finish` -- "do NOT emit `finish` again"
            // in the same breath is the same contradiction this function exists to avoid.
            if looped != finish {
                out.push_str(&format!("Do NOT emit `{looped}` again."));
            } else {
                out.push_str(
                    "Your last `finish` was rejected or empty: put the actual answer in \
                     `summary` this time.",
                );
            }
            return out;
        }
        (Some(w), anchored) => {
            out.push_str(&format!(
                "- If the source file the tests need does not exist yet, create it with \
                 `{w}` (path + the ENTIRE file contents in one shot).\n"
            ));
            match anchored {
                Some(a) => out.push_str(&format!(
                    "- If it exists but a test is failing, fix it: use `{a}` for a small \
                     targeted change, or `{w}` with the ENTIRE corrected contents to rewrite \
                     it wholesale"
                )),
                None => out.push_str(&format!(
                    "- If it exists but a test is failing, fix it: `{w}` with the ENTIRE \
                     corrected contents"
                )),
            }
        }
        (None, Some(a)) => {
            out.push_str(&format!(
                "- If a test is failing, fix it with `{a}` (a small targeted change)"
            ));
        }
    }
    match verify {
        Some(v) => out.push_str(&format!(", then `{v}`.\n")),
        None => out.push_str(".\n"),
    }
    // `whole`/`anchored` already have `looped` filtered out, so this list can never name
    // the tool the next line forbids.
    let acts: Vec<String> = [whole, anchored]
        .into_iter()
        .flatten()
        .map(|t| format!("`{t}`"))
        .collect();
    out.push_str(&format!(
        "Emit {} (an action that changes the workspace) this turn. Do NOT emit `{looped}` again.",
        acts.join(" or ")
    ));
    out
}

/// Consult the advisor (senior) for a hint, formatted as guidance to inject.
/// `None` when there's no advisor or it couldn't help. Not called directly by the
/// loop: every consult goes through [`super::stall::Interventions::consult`], which
/// is where the per-run budget ([`ADVISOR_LIMIT`]) is spent.
pub(super) fn escalate(
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

/// Build a non-finished stop report. `verified` is the last verification the run log saw
/// (see [`crate::runlog::RunLog::last_verification_green`]) -- the suite is NOT re-run to
/// fill it: the harness already has that answer, and a re-run cost a subprocess per stop and
/// could disagree with what the model was shown.
#[allow(clippy::too_many_arguments)]
pub(super) fn stopped(
    reason: StopReason,
    steps: usize,
    verified: Option<bool>,
    // The pre-run baseline (see `AgentReport::started_green`), measured once before the
    // first turn and carried through unchanged.
    started_green: Option<bool>,
    journal: &Journal,
    metrics: ToolCallMetrics,
    peak_prompt_tokens: usize,
    total_prompt_tokens: usize,
    total_cached_prompt_tokens: usize,
    total_prefilled_prompt_tokens: usize,
    peak_reply_tokens: usize,
    harness_faults: Vec<(crate::event::FaultKind, usize)>,
    prompt_budget: usize,
    interventions: usize,
) -> AgentReport {
    AgentReport {
        finished: false,
        steps,
        metrics,
        peak_prompt_tokens,
        total_prompt_tokens,
        total_cached_prompt_tokens,
        total_prefilled_prompt_tokens,
        peak_reply_tokens,
        harness_faults,
        prompt_budget,
        verified,
        started_green,
        change_summary: journal.change_summary(),
        stop_reason: reason,
        interventions,
    }
}

/// The positive "you may stop now" signal, appended to a GREEN verification on a run that
/// started green AND has actually changed the workspace.
///
/// Measured (evals/results/2026-09-08-refactor-anatomy): on a real refactor the work was
/// complete at model call 35 and the run cost another 246s over calls 36-43 — eight replies
/// that each opened "The build passes. The extraction is complete: …", buried the SAME
/// `cargo check` mid-reply, and ran to the token cap. Six of the commands were byte-identical.
/// Phase 4 removed auto-finish on a green-at-start run (correctly: an unreferenced orphan file
/// was counting as success) but replaced it with a note that only ever says what green does
/// NOT mean — "this does not mean the task is done". A model that has done the work reads that
/// as "keep checking", and checking is free to attempt and expensive to run.
///
/// So this is the other half: green + the workspace demonstrably changed = the harness saying,
/// in one unambiguous line, that re-running the check cannot tell it anything new and `finish`
/// is the move. The harness still does NOT finish for it — the model's own `finish` remains
/// the deliberate act, and `gate_finish` still runs the suite before honouring it.
pub(super) fn done_steer(registry: &ToolRegistry) -> String {
    let finish = mention(registry, &["finish"]).unwrap_or("finish");
    format!(
        "THE WORK IS VERIFIED AND YOU ARE DONE. You changed the workspace this run and the \
         verification above is GREEN. Running the check again will return exactly this — it \
         cannot tell you anything new. Call `{finish}` NOW with a one-line `summary` of what \
         you changed. Do not re-verify, do not re-read, do not restate the work: `{finish}` \
         is the only correct call this turn."
    )
}

/// The same signal, delivered by the stall ladder instead of by an observation.
///
/// When the ladder fires on a run that started green, has changed the workspace, and whose
/// last verification was green, the model is not lost — it is finished and waiting for
/// permission it will never get. The generic [`self_recovery_directive`] tells it to "take a
/// concrete next action", which on a completed refactor means another `cargo check`: the exact
/// loop the ladder is supposed to break. Name the real situation instead.
pub(super) fn finished_stall_directive(registry: &ToolRegistry) -> String {
    let finish = mention(registry, &["finish"]).unwrap_or("finish");
    format!(
        "STOP — you are repeating yourself. The change is MADE and the verification is GREEN. \
         There is nothing left to check: every re-run returns the same green result. Call \
         `{finish}` this turn with a one-line `summary` of the change. Any other tool call is \
         wasted."
    )
}
