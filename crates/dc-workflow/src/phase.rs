//! The six workflow phases (spec 09) and their ordering.
//!
//! The pipeline is fixed: specs → architecture → layout → stage breakdown →
//! implementation plan → work decomposition. Each phase reasons over the prior
//! *approved* artifacts to produce the next, so a tiny model never holds the whole
//! problem at once.

use serde::{Deserialize, Serialize};

/// One stage of the staged workflow, in pipeline order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Phase {
    /// What & why — goals, non-goals, constraints. Always first.
    Specs,
    /// How, high-level — components, boundaries, data flow, key choices.
    Architecture,
    /// Concrete structure — directories, modules, files, responsibilities.
    Layout,
    /// Work split into incremental stages, each defined by its tests (TDD).
    StageBreakdown,
    /// For each stage, the concrete plan to turn its tests red → green.
    ImplementationPlan,
    /// The plan sliced into small independent subtasks — the swarm's input.
    WorkDecomposition,
}

impl Phase {
    /// All phases in pipeline order.
    pub const ALL: [Phase; 6] = [
        Phase::Specs,
        Phase::Architecture,
        Phase::Layout,
        Phase::StageBreakdown,
        Phase::ImplementationPlan,
        Phase::WorkDecomposition,
    ];

    /// The phase after this one, or `None` if this is the last.
    pub fn next(self) -> Option<Phase> {
        let i = self.index();
        Phase::ALL.get(i + 1).copied()
    }

    /// 0-based position in the pipeline (Specs = 0 … WorkDecomposition = 5).
    pub fn index(self) -> usize {
        Phase::ALL.iter().position(|&p| p == self).unwrap()
    }

    /// A short kebab slug used in artifact filenames (`01-specs.md`, …). The
    /// numeric prefix keeps the plan directory ordered on disk.
    pub fn slug(self) -> &'static str {
        match self {
            Phase::Specs => "specs",
            Phase::Architecture => "architecture",
            Phase::Layout => "layout",
            Phase::StageBreakdown => "stage-breakdown",
            Phase::ImplementationPlan => "implementation-plan",
            Phase::WorkDecomposition => "work-decomposition",
        }
    }

    /// The on-disk filename for this phase's artifact, e.g. `01-specs.md`.
    pub fn filename(self) -> String {
        format!("{:02}-{}.md", self.index() + 1, self.slug())
    }

    /// A human title for the phase.
    pub fn title(self) -> &'static str {
        match self {
            Phase::Specs => "Specs",
            Phase::Architecture => "Architecture",
            Phase::Layout => "Layout",
            Phase::StageBreakdown => "Stage breakdown (test-first)",
            Phase::ImplementationPlan => "Implementation plan",
            Phase::WorkDecomposition => "Work decomposition",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_ordered_and_chain() {
        assert_eq!(Phase::Specs.index(), 0);
        assert_eq!(Phase::WorkDecomposition.index(), 5);
        assert_eq!(Phase::Specs.next(), Some(Phase::Architecture));
        assert_eq!(Phase::WorkDecomposition.next(), None);
        // Walking `next` visits all six in order.
        let mut p = Some(Phase::Specs);
        let mut seen = Vec::new();
        while let Some(cur) = p {
            seen.push(cur);
            p = cur.next();
        }
        assert_eq!(seen, Phase::ALL.to_vec());
    }

    #[test]
    fn filenames_are_ordered_and_unique() {
        let names: Vec<String> = Phase::ALL.iter().map(|p| p.filename()).collect();
        assert_eq!(names[0], "01-specs.md");
        assert_eq!(names[5], "06-work-decomposition.md");
        // Sorting the filenames preserves pipeline order (numeric prefix).
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, names);
    }
}
