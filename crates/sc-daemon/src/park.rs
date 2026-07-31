//! The gate that never approves.
//!
//! > **The runner runs phases. It never passes a gate.** It works up to the first
//! > checkpoint the ceremony demands, parks, and waits. A parked run is the system
//! > working correctly, not a run that failed to finish. (spec 19)
//!
//! ## Why this needs no `sc-core` rewrite
//!
//! Spec 19 warns at length that making *confirmations* parkable would mean turning
//! the agent loop into a resumable state machine, because a confirmation blocks
//! mid-loop over a thread's stack — the model conversation, turn history, plan,
//! stall counters and four trait objects, none of which serialize.
//!
//! A **gate** is a different thing entirely. It is called at a phase boundary
//! where the whole recoverable state is a serializable `WorkflowState` already on
//! disk — which is exactly why resume works. And the vocabulary for "stop here,
//! keep what was approved" exists: [`Decision::Abort`]. So parking is a `Gate`
//! implementation, and the run unwinds through the ordinary path.
//!
//! ## Why it is a gate rather than a flag
//!
//! Because a `Gate` is the *only* thing the runner consults, a daemon that owns
//! this one cannot approve anything, under any ceremony, by any code path. Spec 19
//! lists "no self-approval" first among its anti-goals — not behind a flag, not
//! for `minimal` ceremony, not for "trivial" tasks — and putting the refusal in
//! the seat where approval would otherwise happen is what makes that structural.

use std::sync::Mutex;

use sc_workflow::{Artifact, Decision, Gate, Phase, PhaseSet};

/// A gate that parks instead of deciding.
///
/// Every phase the ceremony gates returns [`Decision::Abort`], recording which
/// phase stopped the run. Phases the ceremony does *not* gate pass through
/// untouched — that is the ceremony's job, not this gate's (spec 09: ceremony
/// chooses which phases gate; it never chooses to skip a human at a gate that
/// exists).
pub struct ParkingGate {
    gated: PhaseSet,
    /// The phase that parked the run, once one has.
    parked_at: Mutex<Option<Phase>>,
}

impl ParkingGate {
    /// Park at any phase in `gated`.
    pub fn new(gated: PhaseSet) -> Self {
        Self {
            gated,
            parked_at: Mutex::new(None),
        }
    }

    /// Which phase parked the run, if one did.
    ///
    /// `None` after a run that reached its stopping point without meeting a gated
    /// phase — the ceremony gated nothing on the way.
    pub fn parked_at(&self) -> Option<Phase> {
        *self.parked_at.lock().unwrap()
    }

    /// Did this run park?
    pub fn parked(&self) -> bool {
        self.parked_at().is_some()
    }
}

impl Gate for ParkingGate {
    fn decide(&self, phase: Phase, _artifact: &Artifact) -> Decision {
        if !self.gated.contains(phase) {
            // Not a checkpoint under this ceremony: advance, exactly as
            // `CeremonyGate` would.
            return Decision::Approve;
        }
        // A human is required here, and there is no human. Stop and keep what was
        // approved — the artifact is already on disk as a draft, so resume shows
        // this same one rather than regenerating a different one.
        //
        // Only the FIRST gated phase is recorded: that is where the run actually
        // stopped, and a later overwrite would misreport it.
        let mut at = self.parked_at.lock().unwrap();
        if at.is_none() {
            *at = Some(phase);
        }
        Decision::Abort
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_workflow::Ceremony;

    fn artifact(phase: Phase) -> Artifact {
        Artifact::draft(phase, "# drafted")
    }

    #[test]
    fn a_gated_phase_parks_rather_than_approving() {
        let gate = ParkingGate::new(PhaseSet::of([Phase::Specs]));
        assert_eq!(
            gate.decide(Phase::Specs, &artifact(Phase::Specs)),
            Decision::Abort
        );
        assert_eq!(gate.parked_at(), Some(Phase::Specs));
        assert!(gate.parked());
    }

    #[test]
    fn an_ungated_phase_advances() {
        // Ceremony decides WHICH phases gate; this gate decides what happens at
        // one. Blocking everything would make ceremony meaningless.
        let gate = ParkingGate::new(PhaseSet::of([Phase::WorkDecomposition]));
        assert_eq!(
            gate.decide(Phase::Specs, &artifact(Phase::Specs)),
            Decision::Approve
        );
        assert!(!gate.parked());
    }

    #[test]
    fn no_ceremony_can_make_this_gate_approve_a_gated_phase() {
        // Spec 19's first anti-goal: no self-approval. Not behind a flag, not for
        // `minimal`, not for "trivial" tasks. Every ceremony, every phase it
        // gates, parks.
        for ceremony in [Ceremony::Minimal, Ceremony::Standard, Ceremony::Full] {
            let gated = ceremony.gates();
            let gate = ParkingGate::new(gated);
            for phase in Phase::ALL {
                let decision = gate.decide(phase, &artifact(phase));
                if gated.contains(phase) {
                    assert_eq!(
                        decision,
                        Decision::Abort,
                        "{ceremony:?} gates {phase:?} — it must park"
                    );
                }
            }
        }
    }

    #[test]
    fn the_recorded_phase_is_where_the_run_actually_stopped() {
        // The runner stops at the first Abort, so a later phase overwriting the
        // record would misreport where the human is needed.
        let gate = ParkingGate::new(PhaseSet::of([Phase::Specs, Phase::Architecture]));
        gate.decide(Phase::Specs, &artifact(Phase::Specs));
        gate.decide(Phase::Architecture, &artifact(Phase::Architecture));
        assert_eq!(gate.parked_at(), Some(Phase::Specs));
    }

    #[test]
    fn a_gate_that_never_fires_reports_no_park() {
        // The run reached its stopping point without needing anyone.
        let gate = ParkingGate::new(PhaseSet::default());
        for phase in Phase::ALL {
            assert_eq!(gate.decide(phase, &artifact(phase)), Decision::Approve);
        }
        assert!(!gate.parked());
        assert_eq!(gate.parked_at(), None);
    }

    #[test]
    fn parking_never_yields_revise_or_send_back() {
        // Those decisions mean a human looked. Returning one would fabricate a
        // review that did not happen — worse than refusing, because the run would
        // proceed as though someone had read it.
        let gate = ParkingGate::new(PhaseSet::of(Phase::ALL));
        for phase in Phase::ALL {
            let d = gate.decide(phase, &artifact(phase));
            assert!(
                matches!(d, Decision::Abort),
                "{phase:?} produced {d:?}, which claims a human acted"
            );
        }
    }
}
