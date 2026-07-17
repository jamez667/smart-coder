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
        }
    }
}

impl Plan {
    /// Fold one phase artifact into the plan. `tests_written` is non-empty on the
    /// post-stage-breakdown event that reports the frozen tests.
    pub fn apply(&mut self, phase: Phase, content: &str, tests_written: &[String]) {
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
        p.apply(Phase::Specs, "spec", &[]);
        assert_eq!(p.current_phase(), Some(Phase::Architecture));
        // Complete every phase → no current phase (planning done).
        for ph in Phase::ALL {
            p.apply(ph, "x", &[]);
        }
        assert_eq!(p.current_phase(), None);
    }

    #[test]
    fn applying_a_phase_marks_it_done_with_content() {
        let mut p = Plan::default();
        p.apply(Phase::Specs, "  ## Goals\nbuild a thing  ", &[]);
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
        p.apply(Phase::WorkDecomposition, json, &[]);
        assert_eq!(p.subtasks, vec!["write the parser", "add the lexer"]);
        // And the raw artifact is still stored on the step.
        assert!(p.steps()[5].done);
    }

    #[test]
    fn tests_written_event_records_frozen_tests_without_clobbering_a_phase() {
        let mut p = Plan::default();
        p.apply(Phase::StageBreakdown, "the coverage plan", &[]);
        p.apply(
            Phase::StageBreakdown,
            "frozen tests written:\ntest_a.py",
            &["test_a.py".to_string()],
        );
        assert_eq!(p.frozen_tests, vec!["test_a.py"]);
        // The phase content from the real artifact is preserved, not overwritten.
        assert_eq!(p.steps()[3].content, "the coverage plan");
        assert!(p.started());
    }

    #[test]
    fn malformed_decomposition_yields_no_subtasks_not_a_panic() {
        let mut p = Plan::default();
        p.apply(Phase::WorkDecomposition, "no json here", &[]);
        assert!(p.subtasks.is_empty());
    }
}
