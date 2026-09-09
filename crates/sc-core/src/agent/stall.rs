//! The stall-recovery ladder (spec 03 — the harness owns recovery).
//!
//! When the [`StallDetector`] reports the model is looping or making no progress, the loop
//! hands control here. The ladder tries, in order: a root-cause **diagnosis** (opt-in, a
//! focused debugger pass over the stored failing output), then an advisor **escalation**
//! ("junior asks senior"), then a bounded advisor-free **self-recovery** directive, and
//! finally gives up. Each rung that fires injects an observation and resets the stall so the
//! model gets a fresh turn; only the last rung stops the run.
//!
//! The intervention bookkeeping (how many diagnoses/advisor consults/self-recoveries we've
//! spent, and the running intervention count) lives in [`Interventions`], threaded through
//! the whole loop.

use sc_context::TurnRecord;
use sc_model::ModelBackend;
use sc_tools::ToolRegistry;

use crate::event::{AgentEvent, EventSink};
use crate::plan::PlanState;
use crate::recovery::{Progress, StallDetector};
use crate::runlog::RunLogSink;

use super::escalation::{
    escalate, finished_stall_directive, recent_tools, self_recovery_directive, ADVISOR_LIMIT,
    DIAGNOSIS_LIMIT, SELF_RECOVERY_LIMIT,
};
use super::prompt::gather_sources;
use super::window::RecentWindow;
use super::AgentConfig;

/// Running counts of the harness's in-loop interventions.
#[derive(Default)]
pub(super) struct Interventions {
    /// Total interventions this run (advice, nudges, diagnoses) — reported as `interventions`.
    pub(super) count: usize,
    /// Root-cause diagnoses run this run, bounded by [`DIAGNOSIS_LIMIT`].
    pub(super) diagnoses: usize,
    /// Advisor-free self-recovery directives issued, bounded by [`SELF_RECOVERY_LIMIT`].
    pub(super) self_recoveries: usize,
    /// Advisor consultations, bounded by [`ADVISOR_LIMIT`].
    ///
    /// This rung was the ONLY unbounded one: the diagnosis rung above it checks
    /// `DIAGNOSIS_LIMIT` and the self-recovery rung below it checks
    /// `SELF_RECOVERY_LIMIT`, but a successful advisor call just reset the stall and
    /// returned `Recovered` with nothing counting it. So a run with an advisor
    /// configured consulted the senior on EVERY stall until `max_steps` -- unbounded
    /// spend on the expensive T1 model -- and `StopReason::Stalled` was unreachable
    /// for that configuration.
    pub(super) advisor_nudges: usize,
    /// Consecutive ladder interventions that were followed by NO change to the workspace.
    ///
    /// Every rung below resets the stall detector so the model gets a clean turn, which is
    /// right the first time: a firm directive often does land. It is wrong the third time.
    /// Reset-then-repeat is a cycle -- repeat, advise, reset, repeat -- and the run only
    /// escapes it when the bounded rungs run out, at a full prompt pass plus a
    /// maximum-length generation per round. Measured: the 598s refactor spent 246s of it
    /// here, and hit the step cap rather than the ladder's end.
    ///
    /// Counts intervention-to-intervention, not turn-to-turn: it is incremented when the
    /// ladder fires again with `changed_since_intervention` still false, and cleared the
    /// moment any turn changes the workspace. Advice that produces work is unbounded, as
    /// before; only advice that produces nothing is capped.
    pub(super) fruitless_stalls: usize,
    /// Whether the workspace has changed since the last rung fired. Set false when a rung
    /// injects a directive; set true by the loop on any turn that changes bytes.
    pub(super) changed_since_intervention: bool,
}

/// How many times the ladder may fire WITHOUT the workspace changing in between before it
/// stops resetting and gives up.
///
/// Two, because the first intervention is the harness's honest attempt and the second is its
/// confirmation that the attempt did not land. A third round buys nothing a second did not,
/// and each round costs a full prompt pass plus a maximum-length generation -- 30s+ on a 35B
/// model, and rising, since every wasted reply is appended to the prompt.
pub(super) const FRUITLESS_STALL_LIMIT: usize = 2;

impl Interventions {
    /// Consult the advisor (junior asks senior, spec 02), spending one unit of the run's
    /// advisor budget. The ONE place `escalate` is reached from: `ask_user` and the stall
    /// ladder both come through here, so [`ADVISOR_LIMIT`] bounds the senior's total spend
    /// however the consults are triggered. `None` when the budget is spent, there is no
    /// advisor, or it had nothing to say -- the caller then falls back to something bounded
    /// of its own.
    pub(super) fn consult(
        &mut self,
        advisor: Option<&dyn ModelBackend>,
        task: &str,
        plan: &PlanState,
        history: &[TurnRecord],
        trigger: &str,
    ) -> Option<String> {
        if self.advisor_nudges >= ADVISOR_LIMIT {
            return None;
        }
        let advice = escalate(advisor, task, plan, history, trigger)?;
        self.advisor_nudges += 1;
        self.count += 1;
        Some(advice)
    }
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
    step: usize,
    action: u64,
    changed: bool,
    registry: &ToolRegistry,
    interv: &mut Interventions,
    stall: &mut StallDetector,
    recent: &mut RecentWindow,
    history: &[TurnRecord],
    plan: &PlanState,
    cfg: &AgentConfig,
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    instruction: &str,
    workspace: &std::path::Path,
    // The run started green, has since changed the workspace, and the last verification came
    // back green: the model is not lost, it is FINISHED and waiting for a permission it will
    // never get. Changes what the ladder says (see `finished_stall_directive`) -- the generic
    // directive's "take a concrete next action" is, on a completed refactor, another
    // verification, which is the very loop the ladder was called to break.
    finished_shape: bool,
    runlog: &RunLogSink,
    sink: &dyn EventSink,
) -> StallDecision {
    // Any turn that touched bytes clears the fruitless streak, stalled or not: advice that
    // produced work has landed, and the next stall deserves the full ladder again.
    if changed {
        interv.changed_since_intervention = true;
        interv.fruitless_stalls = 0;
    }
    let stuck = match stall.observe(action, changed, cfg.repeat_limit, cfg.no_progress_limit) {
        Progress::Ok => return StallDecision::Continue,
        Progress::Looping => "repeating the same action without progress",
        Progress::Stuck => "many turns with no change to the workspace",
    };
    sink.record(&AgentEvent::Stalled {
        trigger: stuck.to_string(),
    });

    // The ladder has already intervened and NOTHING moved since. Every rung below ends in
    // `stall.reset()`, so without this the run cycles -- repeat, advise, reset, repeat --
    // and escapes only when the bounded rungs are spent, one full prompt pass plus one
    // maximum-length generation per round. Stop paying for advice that demonstrably is not
    // landing (see `FRUITLESS_STALL_LIMIT`).
    // `interv.count > 0` gates it on a rung having actually fired: before the first
    // intervention there is nothing to judge as fruitless, and the flag's `false` default
    // would otherwise credit the very first stall as a failed round.
    if interv.count > 0 && !interv.changed_since_intervention {
        interv.fruitless_stalls += 1;
        if interv.fruitless_stalls >= FRUITLESS_STALL_LIMIT {
            return StallDecision::GiveUp(crate::recovery::StopReason::Stalled(format!(
                "{stuck}; the harness intervened {} times with no change to the workspace \
                 between them",
                interv.fruitless_stalls
            )));
        }
    }

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
            interv.changed_since_intervention = false;
            stall.reset();
            sink.record(&AgentEvent::Diagnosis {
                trigger: stuck.to_string(),
                report: report.clone(),
            });
            recent.push_observation(&crate::diagnose::diagnosis_observation(&report));
            return StallDecision::Recovered;
        }
    }

    // Junior asks senior for a nudge (spec 02). With no advisor (the single-model setup) or
    // the advisor budget spent, the harness steers the model back in-band a bounded number
    // of times before giving up — a capable model just needs a firm directive, not a senior.
    // A model that has done the work and verified it does not need a senior, and does not
    // need to be told to "take a concrete next action" -- that is precisely what it thinks
    // it is doing. It needs to be told it may stop. Take this rung ahead of both the advisor
    // (an expensive T1 call that would arrive at the same answer) and the generic directive.
    if finished_shape {
        interv.self_recoveries += 1;
        interv.count += 1;
        interv.changed_since_intervention = false;
        stall.reset();
        let advice = finished_stall_directive(registry);
        super::report_if_unoffered(&advice, registry, step, sink);
        sink.record(&AgentEvent::Advice {
            trigger: stuck.to_string(),
            advice: advice.clone(),
        });
        recent.push_observation(&advice);
        return StallDecision::Recovered;
    }

    match interv.consult(advisor, instruction, plan, history, stuck) {
        Some(advice) => {
            interv.changed_since_intervention = false;
            stall.reset();
            sink.record(&AgentEvent::Advice {
                trigger: stuck.to_string(),
                advice: advice.clone(),
            });
            recent.push_observation(&advice);
            StallDecision::Recovered
        }
        None if interv.self_recoveries < SELF_RECOVERY_LIMIT => {
            interv.self_recoveries += 1;
            interv.count += 1;
            interv.changed_since_intervention = false;
            stall.reset();
            // `recent.evicted()` decides whether the directive may claim the model still
            // has everything it read: once turns have been compacted away, it does not.
            let advice =
                self_recovery_directive(&recent_tools(history), registry, recent.evicted());
            super::report_if_unoffered(&advice, registry, step, sink);
            sink.record(&AgentEvent::Advice {
                trigger: stuck.to_string(),
                advice: advice.clone(),
            });
            recent.push_observation(&advice);
            StallDecision::Recovered
        }
        None => StallDecision::GiveUp(crate::recovery::StopReason::Stalled(stuck.to_string())),
    }
}
