//! Stall-recovery helpers: the advisor-free self-recovery directive a single model gets
//! when it loops, the advisor consult ("junior asks senior"), and the non-finished stop
//! report the loop returns when it gives up.

use std::path::Path;

use sc_context::{summarize_history, TurnRecord};
use sc_model::ModelBackend;
use sc_tools::Journal;

use crate::advisor::{advice_observation, consult, Predicament};
use crate::metrics::ToolCallMetrics;
use crate::plan::PlanState;
use crate::recovery::StopReason;

use super::AgentReport;

/// How many times the harness self-recovers from a stall WITHOUT an advisor before giving up.
pub(super) const SELF_RECOVERY_LIMIT: usize = 2;

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

/// A firm, advisor-free recovery instruction injected when a single model stalls.
/// Unlike the gentle repeat-nudge, this names the loop and gives the model a
/// concrete decision: if you've read what you need, EDIT now; if the suite is the
/// blocker, fix the failure it reported. The model has no senior to ask, so the
/// harness has to be the one that breaks the loop.
pub(super) fn self_recovery_directive(recent: &[String]) -> String {
    let looped = recent
        .first()
        .map(String::as_str)
        .unwrap_or("the same tool");
    format!(
        "STOP — you are stuck in a loop calling `{looped}` and making no progress. \
         You already have everything you read in the context above; re-reading or \
         re-running changes nothing. Decide the next CONCRETE move right now:\n\
         - If the source file the tests need does not exist yet, create it with \
         `write_file` (path + the ENTIRE file contents in one shot).\n\
         - If it exists but a test is failing, fix it: use `edit_file` for a small \
         anchored change, or `write_file` with the ENTIRE corrected contents to rewrite \
         it wholesale, then `run_verification`.\n\
         Emit a `write_file` or `edit_file` (an action that changes the workspace) this \
         turn. Do NOT emit `{looped}` again."
    )
}

/// Consult the advisor (senior) for a hint, formatted as guidance to inject.
/// `None` when there's no advisor or it couldn't help — the caller then stops.
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

/// Build a non-finished stop report, computing `verified` if a command is set.
#[allow(clippy::too_many_arguments)]
pub(super) fn stopped(
    reason: StopReason,
    steps: usize,
    sandbox: &sc_verify::Sandbox,
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
            .map(|c| sc_verify::run_verification_in(sandbox, workspace, c).all_green()),
        change_summary: journal.change_summary(),
        stop_reason: reason,
        interventions,
    }
}
