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
pub(super) fn self_recovery_directive(recent: &[String], registry: &ToolRegistry) -> String {
    let looped = recent
        .first()
        .map(String::as_str)
        .unwrap_or("the same tool");
    let whole = mention(registry, &["write_file", "create_file"]);
    let anchored = mention(registry, &["edit_file", "edit_lines", "edit_function"]);
    let verify = mention(registry, &["run_verification"]);

    let mut out = format!(
        "STOP — you are stuck in a loop calling `{looped}` and making no progress. \
         You already have everything you read in the context above; re-reading or \
         re-running changes nothing. Decide the next CONCRETE move right now:\n"
    );
    match (whole, anchored) {
        (None, None) => {
            // Nothing here can change the workspace: this is a question, and the only
            // move left is to answer it.
            out.push_str(
                "- If you have the answer, call `finish` NOW with it in `summary`.\n\
                 - If you are reading the wrong file, say so in `finish` rather than reading on.\n",
            );
            out.push_str(&format!("Do NOT emit `{looped}` again."));
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
        change_summary: journal.change_summary(),
        stop_reason: reason,
        interventions,
    }
}
