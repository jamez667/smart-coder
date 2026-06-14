//! Human checkpoints at the phase boundaries (spec 09).
//!
//! At each ⛳ between phases the workflow halts and asks a [`Gate`] what to do with
//! the just-produced artifact. The decision space is the spec's checkpoint table:
//!
//! | Action | Effect |
//! | --- | --- |
//! | **Approve**   | Accept the artifact; proceed to the next phase. |
//! | **Revise**    | The human edited the on-disk file; re-read it and accept. |
//! | **Send back** | Return to this phase (or an earlier one); downstream artifacts are invalidated and regenerated. |
//! | **Abort**     | Stop the workflow; approved artifacts so far are kept. |
//!
//! The gate is **harness-owned** — it lives outside the model, so a model can never
//! self-approve or skip a phase. The runner ([`crate::runner`]) drives the loop and
//! consults the gate; the gate only *decides*. Autonomous runs use [`AutoApprove`],
//! which approves unconditionally; interactive runs supply a gate that reads a human.

use crate::phase::{Phase, PhaseSet};
use crate::state::Artifact;

/// A human's decision at a phase checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Accept the artifact as-is and advance.
    Approve,
    /// The human edited the artifact file on disk; re-read it and accept the edit.
    Revise,
    /// Bounce back to `target` (this phase or an earlier one), optionally with
    /// feedback notes for the regeneration. Downstream artifacts are dropped.
    SendBack {
        target: Phase,
        notes: Option<String>,
    },
    /// Stop the workflow. Approved artifacts so far are kept.
    Abort,
}

/// Decides what to do with a freshly-produced phase artifact at its checkpoint.
///
/// Implementations are the *only* thing standing between a draft and approval, so
/// the model cannot bypass them. Two live: [`AutoApprove`] (autonomous) and an
/// interactive stdin gate in the CLI.
pub trait Gate {
    /// Inspect `artifact` for `phase` and return the human's [`Decision`].
    fn decide(&self, phase: Phase, artifact: &Artifact) -> Decision;
}

/// The autonomous gate: approve every phase unconditionally. This is the default
/// (non-interactive) behavior — prove the spec→…→decomposition chain end to end
/// without a human in the loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApprove;

impl Gate for AutoApprove {
    fn decide(&self, _phase: Phase, _artifact: &Artifact) -> Decision {
        Decision::Approve
    }
}

/// A gating policy that scales the ceremony to the task (spec 09 — "Scaling the
/// ceremony to the task"). Only the phases in `gated` stop at the inner human gate;
/// every other phase auto-approves. This is the adaptive middle ground between
/// [`AutoApprove`] (gate nothing) and gating every phase (full ceremony): the
/// `--ceremony`/`--gates` CLI flags build the [`PhaseSet`], and the runner is none
/// the wiser — it just sees a [`Gate`].
pub struct CeremonyGate<'a> {
    gated: PhaseSet,
    human: &'a dyn Gate,
}

impl<'a> CeremonyGate<'a> {
    /// Gate `gated` through `human`; auto-approve every other phase.
    pub fn new(gated: PhaseSet, human: &'a dyn Gate) -> Self {
        Self { gated, human }
    }
}

impl Gate for CeremonyGate<'_> {
    fn decide(&self, phase: Phase, artifact: &Artifact) -> Decision {
        if self.gated.contains(phase) {
            self.human.decide(phase, artifact)
        } else {
            Decision::Approve
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approve_always_approves() {
        let g = AutoApprove;
        for phase in Phase::ALL {
            let a = Artifact::draft(phase, "x");
            assert_eq!(g.decide(phase, &a), Decision::Approve);
        }
    }

    /// A human gate stub that records which phases it was consulted for and always
    /// returns a fixed decision — so we can prove the ceremony only delegates for
    /// the gated phases.
    struct Recording {
        seen: std::cell::RefCell<Vec<Phase>>,
        reply: Decision,
    }
    impl Gate for Recording {
        fn decide(&self, phase: Phase, _a: &Artifact) -> Decision {
            self.seen.borrow_mut().push(phase);
            self.reply.clone()
        }
    }

    #[test]
    fn ceremony_gate_only_consults_the_human_for_gated_phases() {
        // The inner human would Abort if asked — so any non-gated phase must NOT
        // reach it (it auto-approves instead).
        let human = Recording {
            seen: std::cell::RefCell::new(Vec::new()),
            reply: Decision::Abort,
        };
        let gated = crate::phase::Ceremony::Minimal.gates(); // {WorkDecomposition}
        let ceremony = CeremonyGate::new(gated, &human);

        for phase in Phase::ALL {
            let a = Artifact::draft(phase, "x");
            let decision = ceremony.decide(phase, &a);
            if phase == Phase::WorkDecomposition {
                assert_eq!(decision, Decision::Abort, "gated phase consults the human");
            } else {
                assert_eq!(decision, Decision::Approve, "ungated phase auto-approves");
            }
        }
        // The human saw exactly the one gated phase.
        assert_eq!(human.seen.into_inner(), vec![Phase::WorkDecomposition]);
    }

    #[test]
    fn ceremony_gate_full_set_consults_every_phase() {
        let human = Recording {
            seen: std::cell::RefCell::new(Vec::new()),
            reply: Decision::Approve,
        };
        let ceremony = CeremonyGate::new(crate::phase::Ceremony::Full.gates(), &human);
        for phase in Phase::ALL {
            let a = Artifact::draft(phase, "x");
            assert_eq!(ceremony.decide(phase, &a), Decision::Approve);
        }
        assert_eq!(human.seen.into_inner(), Phase::ALL.to_vec());
    }
}
