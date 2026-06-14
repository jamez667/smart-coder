//! Per-phase thinking control.
//!
//! The orchestrator may be a "thinking" model (chain-of-thought before the visible
//! answer). For the prose planning phases a small model writes the document
//! directly, so the hidden reasoning is pure latency; for the structured (JSON)
//! phases it can help it enumerate edge cases / dependencies. This policy holds an
//! independent think/no-think choice **for each of the six phases**, kept
//! configurable so a beefier setup can simply turn thinking back on per step.

use crate::phase::Phase;

/// Per-phase thinking control: for each phase, whether to suppress the
/// orchestrator's chain-of-thought (append `/no_think`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkPolicy {
    /// One flag per phase, indexed by [`Phase::index`]: `true` = suppress thinking.
    suppress: [bool; 6],
}

impl Default for ThinkPolicy {
    /// The sensible default on a constrained GPU: suppress thinking on the prose
    /// document phases (the model writes them directly), keep it on for the two
    /// structured (JSON) phases that reason about edge cases / dependencies.
    fn default() -> Self {
        let mut p = ThinkPolicy {
            suppress: [false; 6],
        };
        for phase in Phase::ALL {
            p.suppress[phase.index()] = !phase.is_reasoning();
        }
        p
    }
}

impl ThinkPolicy {
    /// Suppress thinking on every phase (fastest).
    pub fn never_think() -> Self {
        ThinkPolicy {
            suppress: [true; 6],
        }
    }

    /// Think on every phase (best when compute is plentiful / a strong reasoner).
    pub fn always_think() -> Self {
        ThinkPolicy {
            suppress: [false; 6],
        }
    }

    /// Set whether `phase` suppresses thinking, returning the updated policy
    /// (builder-style, so a CLI can flip individual steps).
    pub fn with(mut self, phase: Phase, suppress: bool) -> Self {
        self.suppress[phase.index()] = suppress;
        self
    }

    /// Whether `phase` should run with thinking suppressed (`/no_think` appended).
    pub fn suppress(self, phase: Phase) -> bool {
        self.suppress[phase.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_suppresses_only_doc_phases() {
        let p = ThinkPolicy::default();
        assert!(p.suppress(Phase::Specs)); // prose doc → no_think
        assert!(p.suppress(Phase::Layout));
        assert!(!p.suppress(Phase::StageBreakdown)); // JSON reasoning → think
        assert!(!p.suppress(Phase::WorkDecomposition));
    }

    #[test]
    fn always_and_never_are_uniform() {
        for phase in Phase::ALL {
            assert!(!ThinkPolicy::always_think().suppress(phase));
            assert!(ThinkPolicy::never_think().suppress(phase));
        }
    }

    #[test]
    fn with_sets_a_single_phase_independently() {
        // Flip just one step without touching the others.
        let p = ThinkPolicy::always_think().with(Phase::Layout, true);
        assert!(p.suppress(Phase::Layout));
        assert!(!p.suppress(Phase::Specs));
        assert!(!p.suppress(Phase::Architecture));
    }
}
