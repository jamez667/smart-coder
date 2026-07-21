//! The five workflow phases (spec 09) and their ordering.
//!
//! The pipeline is fixed: specs → architecture → layout → stage breakdown →
//! work decomposition. Each phase reasons over the prior *approved* artifacts to
//! produce the next, so a tiny model never holds the whole problem at once. (The
//! stage breakdown now carries the concrete per-stage steps that a separate
//! implementation-plan phase used to add — for small local models the breakdown is
//! already file-level and actionable, so the extra phase just re-chewed it.)

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
    /// Work split into incremental stages, each with its concrete per-stage steps
    /// (the file(s) and ordered edits) — the single design doc a small worker acts on.
    StageBreakdown,
    /// The plan sliced into small independent subtasks — the swarm's input.
    WorkDecomposition,
}

impl Phase {
    /// All phases in pipeline order.
    pub const ALL: [Phase; 5] = [
        Phase::Specs,
        Phase::Architecture,
        Phase::Layout,
        Phase::StageBreakdown,
        Phase::WorkDecomposition,
    ];

    /// The phase after this one, or `None` if this is the last.
    pub fn next(self) -> Option<Phase> {
        let i = self.index();
        Phase::ALL.get(i + 1).copied()
    }

    /// 0-based position in the pipeline (Specs = 0 … WorkDecomposition = 4).
    pub fn index(self) -> usize {
        Phase::ALL.iter().position(|&p| p == self).unwrap()
    }

    /// Whether this phase benefits from chain-of-thought reasoning. The two phases
    /// that emit *structured* output — the coverage test-plan and the work
    /// decomposition — reason about edge cases and dependencies before emitting
    /// JSON; the prose document phases (spec/architecture/layout) the
    /// model can write directly. Used by [`crate::ThinkPolicy`] to decide whether to
    /// suppress thinking per phase.
    pub fn is_reasoning(self) -> bool {
        matches!(self, Phase::StageBreakdown | Phase::WorkDecomposition)
    }

    /// Which *upstream* phases this phase actually needs as grounding. Feeding every
    /// approved artifact into every phase overflows a small model by the later phases
    /// (observed live 2026-06-14: the WorkDecomposition call carried specs+arch+layout+
    /// stage-breakdown and returned empty → no subtasks → nothing built). Each phase
    /// only consumes what it depends on, so the late phases stay within budget:
    /// - architecture grounds on the specs;
    /// - layout grounds on the architecture (which already summarizes the specs);
    /// - stage-breakdown needs the layout (what files exist) + the specs (what to test);
    /// - work decomposition needs ONLY the layout (files) + stage-breakdown (the ordered
    ///   stages and their concrete steps) — NOT the prose specs/architecture, which it
    ///   doesn't act on.
    pub fn needs_upstream(self) -> &'static [Phase] {
        use Phase::*;
        match self {
            Specs => &[],
            Architecture => &[Specs],
            Layout => &[Architecture],
            StageBreakdown => &[Specs, Layout],
            WorkDecomposition => &[Layout, StageBreakdown],
        }
    }

    /// Whether this phase must emit a machine-readable JSON array (the coverage test
    /// plan and the work decomposition). The engine validates these: a JSON phase that
    /// returns only prose (the reasoning model narrating instead of answering) is a
    /// failed attempt, not a usable artifact — retried with thinking suppressed.
    pub fn produces_json(self) -> bool {
        matches!(self, Phase::StageBreakdown | Phase::WorkDecomposition)
    }

    /// A short kebab slug used in artifact filenames (`01-specs.md`, …). The
    /// numeric prefix keeps the plan directory ordered on disk.
    pub fn slug(self) -> &'static str {
        match self {
            Phase::Specs => "specs",
            Phase::Architecture => "architecture",
            Phase::Layout => "layout",
            Phase::StageBreakdown => "stage-breakdown",
            Phase::WorkDecomposition => "work-decomposition",
        }
    }

    /// The phase named by `slug`, if any — the inverse of [`Phase::slug`]. Used by
    /// the CLI to resolve a phase from user input (send-back target, `--gates`).
    pub fn from_slug(slug: &str) -> Option<Phase> {
        Phase::ALL.iter().copied().find(|p| p.slug() == slug)
    }

    /// The on-disk filename for this phase's artifact, e.g. `01-specs.md`.
    pub fn filename(self) -> String {
        format!("{:02}-{}.md", self.index() + 1, self.slug())
    }

    /// The OpenSpec-style filename for the `specs/<slug>/` layout: `spec.md`, `architecture.md`,
    /// `layout.md`, `breakdown.md`, etc. — no number prefix, sits beside the spec.
    pub fn openspec_filename(self) -> &'static str {
        match self {
            Phase::Specs => "spec.md",
            Phase::Architecture => "architecture.md",
            Phase::Layout => "layout.md",
            Phase::StageBreakdown => "breakdown.md",
            Phase::WorkDecomposition => "decomposition.md",
        }
    }

    /// A human title for the phase.
    pub fn title(self) -> &'static str {
        match self {
            Phase::Specs => "Specs",
            Phase::Architecture => "Architecture",
            Phase::Layout => "Layout",
            Phase::StageBreakdown => "Stage breakdown (test-first)",
            Phase::WorkDecomposition => "Work decomposition",
        }
    }
}

/// A set of phases that require a human gate (spec 09 — "Scaling the ceremony").
/// Everything *not* in the set auto-approves, so this is the policy that scales the
/// ceremony to the task. Backed by a fixed-size bitmask over the five phases — order
/// doesn't matter and membership is the only operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhaseSet {
    /// Bit `i` set ⇔ `Phase::ALL[i]` is in the set.
    bits: u8,
}

impl PhaseSet {
    /// The set containing exactly `phases`.
    pub fn of(phases: impl IntoIterator<Item = Phase>) -> Self {
        let mut set = PhaseSet::default();
        for p in phases {
            set.bits |= 1 << p.index();
        }
        set
    }

    /// Whether `phase` is in the set.
    pub fn contains(&self, phase: Phase) -> bool {
        self.bits & (1 << phase.index()) != 0
    }

    /// Whether no phase is gated (a fully autonomous policy).
    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// The gated phases, in pipeline order — for display.
    pub fn phases(&self) -> Vec<Phase> {
        Phase::ALL
            .iter()
            .copied()
            .filter(|p| self.contains(*p))
            .collect()
    }
}

/// Named ceremony tiers (spec 09 — "more gates for broader/destructive scope, fewer
/// for narrow edits"). Each tier names the phases that stop at a human checkpoint;
/// the rest auto-approve. All tiers still run all five phases — only *which* phases
/// gate changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ceremony {
    /// Narrow edits: one final sign-off before the swarm runs.
    Minimal,
    /// The middle ground: confirm the *what* (specs), the frozen *tests*, and the
    /// *decomposition* — skip the prose design gates.
    Standard,
    /// Broad/destructive: gate every phase (equivalent to bare `--interactive`).
    Full,
}

impl Ceremony {
    /// Parse a tier name (`minimal` | `standard` | `full`).
    pub fn parse(s: &str) -> Option<Ceremony> {
        match s {
            "minimal" => Some(Ceremony::Minimal),
            "standard" => Some(Ceremony::Standard),
            "full" => Some(Ceremony::Full),
            _ => None,
        }
    }

    /// A human label for the tier.
    pub fn label(self) -> &'static str {
        match self {
            Ceremony::Minimal => "minimal",
            Ceremony::Standard => "standard",
            Ceremony::Full => "full",
        }
    }

    /// The phases that stop at a human gate for this tier.
    pub fn gates(self) -> PhaseSet {
        match self {
            Ceremony::Minimal => PhaseSet::of([Phase::WorkDecomposition]),
            Ceremony::Standard => PhaseSet::of([
                Phase::Specs,
                Phase::StageBreakdown,
                Phase::WorkDecomposition,
            ]),
            Ceremony::Full => PhaseSet::of(Phase::ALL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_ordered_and_chain() {
        assert_eq!(Phase::Specs.index(), 0);
        assert_eq!(Phase::WorkDecomposition.index(), 4);
        assert_eq!(Phase::Specs.next(), Some(Phase::Architecture));
        assert_eq!(Phase::WorkDecomposition.next(), None);
        // Walking `next` visits all five in order.
        let mut p = Some(Phase::Specs);
        let mut seen = Vec::new();
        while let Some(cur) = p {
            seen.push(cur);
            p = cur.next();
        }
        assert_eq!(seen, Phase::ALL.to_vec());
    }

    #[test]
    fn from_slug_round_trips_and_rejects_garbage() {
        for p in Phase::ALL {
            assert_eq!(Phase::from_slug(p.slug()), Some(p));
        }
        assert_eq!(Phase::from_slug("nonsense"), None);
        assert_eq!(Phase::from_slug(""), None);
    }

    #[test]
    fn phase_set_membership() {
        let s = PhaseSet::of([Phase::Specs, Phase::WorkDecomposition]);
        assert!(s.contains(Phase::Specs));
        assert!(s.contains(Phase::WorkDecomposition));
        assert!(!s.contains(Phase::Architecture));
        assert!(!s.is_empty());
        // `phases()` lists members in pipeline order.
        assert_eq!(s.phases(), vec![Phase::Specs, Phase::WorkDecomposition]);
        assert!(PhaseSet::default().is_empty());
    }

    #[test]
    fn ceremony_tiers_gate_the_expected_phases() {
        // Minimal: only the final sign-off.
        assert_eq!(
            Ceremony::Minimal.gates().phases(),
            vec![Phase::WorkDecomposition]
        );
        // Standard: the what, the tests, the decomposition.
        assert_eq!(
            Ceremony::Standard.gates().phases(),
            vec![
                Phase::Specs,
                Phase::StageBreakdown,
                Phase::WorkDecomposition
            ]
        );
        // Full: every phase.
        assert_eq!(Ceremony::Full.gates().phases(), Phase::ALL.to_vec());
    }

    #[test]
    fn ceremony_parses_and_rejects() {
        assert_eq!(Ceremony::parse("minimal"), Some(Ceremony::Minimal));
        assert_eq!(Ceremony::parse("standard"), Some(Ceremony::Standard));
        assert_eq!(Ceremony::parse("full"), Some(Ceremony::Full));
        assert_eq!(Ceremony::parse("bogus"), None);
    }

    #[test]
    fn openspec_filenames_are_named_and_unique() {
        assert_eq!(Phase::Specs.openspec_filename(), "spec.md");
        assert_eq!(Phase::Architecture.openspec_filename(), "architecture.md");
        assert_eq!(Phase::Layout.openspec_filename(), "layout.md");
        assert_eq!(Phase::StageBreakdown.openspec_filename(), "breakdown.md");
        let names: std::collections::BTreeSet<_> =
            Phase::ALL.iter().map(|p| p.openspec_filename()).collect();
        assert_eq!(names.len(), Phase::ALL.len(), "all distinct");
    }

    #[test]
    fn filenames_are_ordered_and_unique() {
        let names: Vec<String> = Phase::ALL.iter().map(|p| p.filename()).collect();
        assert_eq!(names[0], "01-specs.md");
        assert_eq!(names[4], "05-work-decomposition.md");
        // Sorting the filenames preserves pipeline order (numeric prefix).
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, names);
    }
}
