//! Harness-owned plan state (spec 03 — PLAN).
//!
//! A small model can't hold a long plan in its head, so the *harness* owns it:
//! the model proposes a short ordered step list, but the harness stores it,
//! tracks each step's status, advances it, and decides when to re-plan. The model
//! only ever sees a compact rendering of "where we are" — it never has to keep the
//! whole plan straight itself.

/// The status of a single plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Active,
    Done,
    Failed,
}

impl StepStatus {
    fn glyph(self) -> &'static str {
        match self {
            StepStatus::Pending => "[ ]",
            StepStatus::Active => "[~]",
            StepStatus::Done => "[x]",
            StepStatus::Failed => "[!]",
        }
    }
}

/// One step: a single concrete action, with status and a retry counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub description: String,
    pub status: StepStatus,
    /// How many times this step has been attempted-and-failed (drives the
    /// per-step retry budget, spec 03).
    pub attempts: usize,
}

impl Step {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            status: StepStatus::Pending,
            attempts: 0,
        }
    }
}

/// The ordered plan, owned by the harness. The first non-done step is "active".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanState {
    steps: Vec<Step>,
}

impl PlanState {
    /// Build a plan from an ordered list of step descriptions. Empty descriptions
    /// are dropped; the first step becomes active.
    pub fn from_descriptions<I, S>(descriptions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut steps: Vec<Step> = descriptions
            .into_iter()
            .map(|d| Step::new(d))
            .filter(|s| !s.description.trim().is_empty())
            .collect();
        if let Some(first) = steps.first_mut() {
            first.status = StepStatus::Active;
        }
        Self { steps }
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Index of the active step (the first not-done, not-failed step), if any.
    pub fn active_index(&self) -> Option<usize> {
        self.steps
            .iter()
            .position(|s| matches!(s.status, StepStatus::Active | StepStatus::Pending))
    }

    /// The active step's description.
    pub fn current_step(&self) -> Option<&str> {
        self.active_index()
            .map(|i| self.steps[i].description.as_str())
    }

    /// Whether every step is done (the plan is complete).
    pub fn all_done(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.status == StepStatus::Done)
    }

    /// Mark the active step done and activate the next pending one.
    pub fn complete_active(&mut self) {
        if let Some(i) = self.active_index() {
            self.steps[i].status = StepStatus::Done;
            self.activate_next();
        }
    }

    /// Record a failed attempt on the active step. Returns the new attempt count.
    /// The step stays active (the loop retries) until the caller fails it out.
    pub fn record_attempt(&mut self) -> usize {
        if let Some(i) = self.active_index() {
            self.steps[i].attempts += 1;
            self.steps[i].status = StepStatus::Active;
            self.steps[i].attempts
        } else {
            0
        }
    }

    /// Give up on the active step (retry budget exhausted) and move on.
    pub fn fail_active(&mut self) {
        if let Some(i) = self.active_index() {
            self.steps[i].status = StepStatus::Failed;
            self.activate_next();
        }
    }

    fn activate_next(&mut self) {
        if let Some(i) = self
            .steps
            .iter()
            .position(|s| s.status == StepStatus::Pending)
        {
            self.steps[i].status = StepStatus::Active;
        }
    }

    /// Compact, structured rendering for the prompt (spec 05 — structured state,
    /// not prose). Shows status glyphs so the model reliably knows where it is.
    pub fn render(&self) -> String {
        if self.steps.is_empty() {
            return String::new();
        }
        let mut s = String::from("plan:");
        for (i, step) in self.steps.iter().enumerate() {
            s.push_str(&format!(
                "\n  {} {}. {}",
                step.status.glyph(),
                i + 1,
                step.description
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_first_step_active() {
        let p = PlanState::from_descriptions(["locate X", "edit Y", "verify"]);
        assert_eq!(p.steps().len(), 3);
        assert_eq!(p.steps()[0].status, StepStatus::Active);
        assert_eq!(p.steps()[1].status, StepStatus::Pending);
        assert_eq!(p.current_step(), Some("locate X"));
    }

    #[test]
    fn drops_empty_descriptions() {
        let p = PlanState::from_descriptions(["a", "  ", "", "b"]);
        assert_eq!(p.steps().len(), 2);
    }

    #[test]
    fn completing_advances_to_the_next_step() {
        let mut p = PlanState::from_descriptions(["a", "b"]);
        p.complete_active();
        assert_eq!(p.steps()[0].status, StepStatus::Done);
        assert_eq!(p.current_step(), Some("b"));
        p.complete_active();
        assert!(p.all_done());
        assert_eq!(p.current_step(), None);
    }

    #[test]
    fn attempts_accumulate_then_fail_moves_on() {
        let mut p = PlanState::from_descriptions(["a", "b"]);
        assert_eq!(p.record_attempt(), 1);
        assert_eq!(p.record_attempt(), 2);
        p.fail_active();
        assert_eq!(p.steps()[0].status, StepStatus::Failed);
        assert_eq!(p.current_step(), Some("b"));
    }

    #[test]
    fn render_shows_status_glyphs() {
        let mut p = PlanState::from_descriptions(["locate", "edit"]);
        p.complete_active();
        let r = p.render();
        assert!(r.contains("[x] 1. locate"), "{r}");
        assert!(r.contains("[~] 2. edit"), "{r}");
    }

    #[test]
    fn empty_plan_renders_empty() {
        assert_eq!(PlanState::default().render(), "");
        assert!(!PlanState::default().all_done()); // vacuously not done
    }
}
