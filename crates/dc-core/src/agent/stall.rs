//! The stall-recovery ladder (spec 03 — the harness owns recovery).
//!
//! When the [`StallDetector`] reports the model is looping or making no progress, the loop
//! hands control here. The ladder tries, in order: a root-cause **diagnosis** (opt-in, a
//! focused debugger pass over the stored failing output), then an advisor **escalation**
//! ("junior asks senior"), then a bounded advisor-free **self-recovery** directive, and
//! finally gives up. Each rung that fires injects an observation and resets the stall so the
//! model gets a fresh turn; only the last rung stops the run.
//!
//! The intervention bookkeeping (how many diagnoses/self-recoveries we've spent, the running
//! intervention count, and the previous-action hash the repeat guard clears on recovery) lives
//! in [`Interventions`], threaded through the whole loop.

use dc_context::TurnRecord;
use dc_model::ModelBackend;

use crate::event::{AgentEvent, EventSink};
use crate::plan::PlanState;
use crate::recovery::{Progress, StallDetector};
use crate::runlog::RunLogSink;

use super::escalation::{
    escalate, recent_tools, self_recovery_directive, DIAGNOSIS_LIMIT, SELF_RECOVERY_LIMIT,
};
use super::prompt::gather_sources;
use super::window::push_observation;
use super::AgentConfig;

/// Running counts of the harness's in-loop interventions, plus the previous-action hash the
/// repeat-dedup guard clears whenever a recovery injects a fresh directive.
#[derive(Default)]
pub(super) struct Interventions {
    /// Total interventions this run (advice, nudges, diagnoses) — reported as `interventions`.
    pub(super) count: usize,
    /// Root-cause diagnoses run this run, bounded by [`DIAGNOSIS_LIMIT`].
    pub(super) diagnoses: usize,
    /// Advisor-free self-recovery directives issued, bounded by [`SELF_RECOVERY_LIMIT`].
    pub(super) self_recoveries: usize,
    /// The previous turn's action hash, used by the repeat-dedup guard. Cleared to `None` on a
    /// recovery so the next (steered) action is never mistaken for a repeat.
    pub(super) prev_action: Option<u64>,
}

/// What the loop should do after the stall ladder runs.
pub(super) enum StallDecision {
    /// Not stalled — carry on with the next turn as normal.
    Continue,
    /// A recovery rung fired and injected a directive — skip straight to the next turn.
    Recovered,
    /// Every rung is exhausted; stop the run with this reason.
    GiveUp(crate::recovery::StopReason),
}

/// Observe the turn's progress and, if stalled, walk the recovery ladder. Mutates the
/// intervention counters, the stall detector, and the recent window as rungs fire.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_stall(
    action: u64,
    changed: bool,
    interv: &mut Interventions,
    stall: &mut StallDetector,
    recent: &mut Vec<dc_model::Message>,
    history: &[TurnRecord],
    plan: &PlanState,
    cfg: &AgentConfig,
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    instruction: &str,
    workspace: &std::path::Path,
    runlog: &RunLogSink,
    sink: &dyn EventSink,
) -> StallDecision {
    let stuck = match stall.observe(action, changed, cfg.repeat_limit, cfg.no_progress_limit) {
        Progress::Ok => return StallDecision::Continue,
        Progress::Looping => "repeating the same action without progress",
        Progress::Stuck => "many turns with no change to the workspace",
    };
    sink.record(&AgentEvent::Stalled {
        trigger: stuck.to_string(),
    });

    // Root-cause diagnosis (spec 03 — recovery). Before the generic recovery ladder, on a
    // TEST-driven run, a focused debugger pass reads the FULL test output + every source file
    // and names the real culprit (the model otherwise reacts to a downstream symptom and edits
    // the wrong file). Gated: opt-in flag, a configured verify command, and a bounded count. On
    // success it IS the intervention for this stall — inject it and skip the generic ladder.
    // Read the last RED verification the run log already captured — no re-run (the suite was just
    // run at the auto-verify above; re-running wasted a Docker subprocess per diagnosis and
    // risked a different result). `None` only if no failing verification was recorded, in which
    // case there's nothing to diagnose.
    let stored_failure = runlog.lock().slice_for_diagnosis().map(str::to_owned);
    if let (true, Some(full)) = (
        cfg.diagnose && interv.diagnoses < DIAGNOSIS_LIMIT && cfg.verify_command.is_some(),
        stored_failure,
    ) {
        let sources = gather_sources(workspace);
        if let Some(report) =
            crate::diagnose::diagnose_failure(backend, instruction, &full, &sources)
        {
            interv.diagnoses += 1;
            interv.count += 1;
            stall.reset();
            interv.prev_action = None;
            sink.record(&AgentEvent::Diagnosis {
                trigger: stuck.to_string(),
                report: report.clone(),
            });
            push_observation(
                recent,
                &crate::diagnose::diagnosis_observation(&report),
                cfg.keep_recent_turns,
            );
            return StallDecision::Recovered;
        }
    }

    // Junior asks senior for a nudge (spec 02). With no advisor (the single-model setup), the
    // harness steers the model back in-band a bounded number of times before giving up — a
    // capable model just needs a firm directive, not a senior.
    match escalate(advisor, instruction, plan, history, stuck) {
        Some(advice) => {
            interv.count += 1;
            stall.reset();
            sink.record(&AgentEvent::Advice {
                trigger: stuck.to_string(),
                advice: advice.clone(),
            });
            push_observation(recent, &advice, cfg.keep_recent_turns);
            StallDecision::Recovered
        }
        None if interv.self_recoveries < SELF_RECOVERY_LIMIT => {
            interv.self_recoveries += 1;
            interv.count += 1;
            stall.reset();
            interv.prev_action = None;
            let advice = self_recovery_directive(&recent_tools(history));
            sink.record(&AgentEvent::Advice {
                trigger: stuck.to_string(),
                advice: advice.clone(),
            });
            push_observation(recent, &advice, cfg.keep_recent_turns);
            StallDecision::Recovered
        }
        None => StallDecision::GiveUp(crate::recovery::StopReason::Stalled(stuck.to_string())),
    }
}
