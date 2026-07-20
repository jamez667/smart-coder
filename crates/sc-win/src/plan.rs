//! The staged-workflow plan, folded from [`UiEvent::Phase`](crate::session::UiEvent)
//! artifacts into a readable form for the plan panel — no iced types, so the "what to
//! show" logic is host-tested. The workflow is the TDD pipeline (spec 09/11): specs →
//! architecture → layout → stage breakdown (which WRITES THE TESTS) → implementation
//! plan → work decomposition (the swarm's subtasks).

use sc_workflow::Phase;

/// One step in the plan: a phase, its (trimmed) artifact text, and whether it's landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub phase: Phase,
    pub title: &'static str,
    /// The artifact text the model produced for this phase. Empty until it arrives.
    pub content: String,
    pub done: bool,
}

/// The accumulated plan across the six phases, plus the frozen tests and the parsed
/// subtask goals from the final decomposition.
#[derive(Debug, Clone)]
pub struct Plan {
    steps: Vec<PlanStep>,
    /// Frozen test files written after the stage-breakdown phase (the TDD contract).
    pub frozen_tests: Vec<String>,
    /// The swarm subtasks parsed from the work-decomposition phase (readable goals).
    pub subtasks: Vec<String>,
    /// The WORKSPACE-RELATIVE directory the phase artifacts are written to (e.g.
    /// `specs/alt-seats`), learned from the first phase event that carries it. This is the
    /// source of truth for each step's on-disk FILE PATH: the master list opens a row's file
    /// in the code view and harvests line-comments on it — both need the path. `None` until a
    /// phase event supplies it (older numbered-plan runs may never set it, and their rows then
    /// aren't clickable — see `PlanStep::path`).
    dir: Option<String>,
}

impl Default for Plan {
    fn default() -> Self {
        let steps = Phase::ALL
            .iter()
            .map(|&phase| PlanStep {
                phase,
                title: phase.title(),
                content: String::new(),
                done: false,
            })
            .collect();
        Self {
            steps,
            frozen_tests: Vec::new(),
            subtasks: Vec::new(),
            dir: None,
        }
    }
}

impl Plan {
    /// Fold one phase artifact into the plan. `tests_written` is non-empty on the
    /// post-stage-breakdown event that reports the frozen tests. `dir` is the
    /// workspace-relative artifact directory (e.g. `specs/alt-seats`); it's the same on
    /// every phase event of a run, so we latch it the first time it's non-empty — it lets
    /// each step resolve its FILE PATH (see [`PlanStep::path`]).
    pub fn apply(&mut self, phase: Phase, content: &str, tests_written: &[String], dir: Option<&str>) {
        // Latch the artifact dir the first time a phase event carries it (source of truth for
        // per-step file paths). Every event of a run carries the same dir, so once is enough.
        if self.dir.is_none() {
            if let Some(d) = dir.map(str::trim).filter(|d| !d.is_empty()) {
                self.dir = Some(d.trim_end_matches('/').to_string());
            }
        }
        if !tests_written.is_empty() {
            self.frozen_tests = tests_written.to_vec();
            return; // The "tests written" event isn't a phase artifact to overwrite.
        }
        if let Some(step) = self.steps.iter_mut().find(|s| s.phase == phase) {
            step.content = content.trim().to_string();
            step.done = true;
        }
        if phase == Phase::WorkDecomposition {
            self.subtasks = parse_subtask_goals(content);
        }
    }

    /// The workspace-relative artifact directory (e.g. `specs/alt-seats`), once known.
    pub fn dir(&self) -> Option<&str> {
        self.dir.as_deref()
    }

    /// The workspace-relative FILE PATH for a phase's artifact (e.g. `specs/alt-seats/spec.md`),
    /// or `None` if the artifact dir isn't known yet. The filename is the OpenSpec name
    /// ([`Phase::openspec_filename`]) — the StagedBuild `specs/<slug>/` layout. This is what the
    /// master list opens in the code view and what send-back harvests comments by.
    pub fn path_for(&self, phase: Phase) -> Option<String> {
        self.dir
            .as_deref()
            .map(|d| format!("{d}/{}", phase.openspec_filename()))
    }

    /// The six steps in pipeline order.
    pub fn steps(&self) -> &[PlanStep] {
        &self.steps
    }

    /// Whether any phase has landed yet (so the panel knows to show the plan).
    pub fn started(&self) -> bool {
        self.steps.iter().any(|s| s.done) || !self.frozen_tests.is_empty()
    }

    /// The phase the workflow is currently *on*: the first not-yet-done step. `None`
    /// once every phase has landed (planning complete — the build is implementing).
    /// Drives the step-flow highlight at the top.
    pub fn current_phase(&self) -> Option<Phase> {
        self.steps.iter().find(|s| !s.done).map(|s| s.phase)
    }
}

/// Pull readable subtask goals out of the work-decomposition JSON array. Tolerant: a
/// small model wraps the array in prose or fences, and may omit fields — we take the
/// `goal` of each object, skipping anything unparseable, so the panel shows a clean
/// numbered list instead of raw JSON.
fn parse_subtask_goals(content: &str) -> Vec<String> {
    let Some(start) = content.find('[') else {
        return Vec::new();
    };
    let Some(end) = content.rfind(']') else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content[start..=end]) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            item.get("goal")
                .and_then(|g| g.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_all_six_phases_in_order_none_done() {
        let p = Plan::default();
        assert_eq!(p.steps().len(), 6);
        assert_eq!(p.steps()[0].phase, Phase::Specs);
        assert_eq!(p.steps()[5].phase, Phase::WorkDecomposition);
        assert!(p.steps().iter().all(|s| !s.done));
        assert!(!p.started());
    }

    #[test]
    fn current_phase_is_the_first_not_done_then_none_when_complete() {
        let mut p = Plan::default();
        assert_eq!(p.current_phase(), Some(Phase::Specs));
        p.apply(Phase::Specs, "spec", &[], None);
        assert_eq!(p.current_phase(), Some(Phase::Architecture));
        // Complete every phase → no current phase (planning done).
        for ph in Phase::ALL {
            p.apply(ph, "x", &[], None);
        }
        assert_eq!(p.current_phase(), None);
    }

    #[test]
    fn applying_a_phase_marks_it_done_with_content() {
        let mut p = Plan::default();
        p.apply(Phase::Specs, "  ## Goals\nbuild a thing  ", &[], None);
        let specs = &p.steps()[0];
        assert!(specs.done);
        assert_eq!(specs.content, "## Goals\nbuild a thing");
        assert!(p.started());
        // Other phases still pending.
        assert!(!p.steps()[1].done);
    }

    #[test]
    fn work_decomposition_parses_readable_subtask_goals() {
        let mut p = Plan::default();
        let json = r#"Here you go:
        [{"id":"t1","goal":"write the parser","files":["p.py"]},
         {"id":"t2","goal":"add the lexer","files":["l.py"]}]"#;
        p.apply(Phase::WorkDecomposition, json, &[], None);
        assert_eq!(p.subtasks, vec!["write the parser", "add the lexer"]);
        // And the raw artifact is still stored on the step.
        assert!(p.steps()[5].done);
    }

    #[test]
    fn tests_written_event_records_frozen_tests_without_clobbering_a_phase() {
        let mut p = Plan::default();
        p.apply(Phase::StageBreakdown, "the coverage plan", &[], None);
        p.apply(
            Phase::StageBreakdown,
            "frozen tests written:\ntest_a.py",
            &["test_a.py".to_string()],
            None,
        );
        assert_eq!(p.frozen_tests, vec!["test_a.py"]);
        // The phase content from the real artifact is preserved, not overwritten.
        assert_eq!(p.steps()[3].content, "the coverage plan");
        assert!(p.started());
    }

    #[test]
    fn malformed_decomposition_yields_no_subtasks_not_a_panic() {
        let mut p = Plan::default();
        p.apply(Phase::WorkDecomposition, "no json here", &[], None);
        assert!(p.subtasks.is_empty());
    }

    #[test]
    fn artifact_dir_is_latched_and_gives_openspec_paths() {
        let mut p = Plan::default();
        // No dir known → no path (older numbered-plan runs: rows not clickable).
        assert_eq!(p.dir(), None);
        assert_eq!(p.path_for(Phase::Specs), None);
        // First phase event carries the workspace-relative dir; a trailing slash is trimmed.
        p.apply(Phase::Specs, "spec", &[], Some("specs/alt-seats/"));
        assert_eq!(p.dir(), Some("specs/alt-seats"));
        // Each phase resolves to its OpenSpec filename beside the spec.
        assert_eq!(p.path_for(Phase::Specs).as_deref(), Some("specs/alt-seats/spec.md"));
        assert_eq!(
            p.path_for(Phase::Architecture).as_deref(),
            Some("specs/alt-seats/architecture.md")
        );
        assert_eq!(
            p.path_for(Phase::WorkDecomposition).as_deref(),
            Some("specs/alt-seats/decomposition.md")
        );
        // The dir is latched: a later event with a different (or empty) dir doesn't move it.
        p.apply(Phase::Architecture, "arch", &[], Some("specs/other"));
        assert_eq!(p.dir(), Some("specs/alt-seats"));
        p.apply(Phase::Layout, "layout", &[], None);
        assert_eq!(p.dir(), Some("specs/alt-seats"));
    }

    #[test]
    fn empty_dir_string_does_not_latch() {
        let mut p = Plan::default();
        p.apply(Phase::Specs, "spec", &[], Some(""));
        assert_eq!(p.dir(), None, "an empty dir is treated as unknown");
    }
}
