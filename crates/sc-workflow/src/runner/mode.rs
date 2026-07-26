//! What a run does ([`WorkflowMode`]) and what it yields ([`WorkflowOutcome`]).

use sc_swarm::TaskBoard;

use crate::phase::Phase;
use crate::state::WorkflowState;

/// How much of the pipeline to run, and whether to write frozen tests.
///
/// The default ([`WorkflowMode::full_tdd`]) is the original behavior: all six phases, and the
/// approved stage-breakdown drives worker-written frozen tests. [`WorkflowMode::plan_only`] is
/// the "structured design, no TDD" mode the desktop app's Execute uses: run the design phases
/// through the stage breakdown, then STOP — no test writing, no decomposition, no build — so
/// the user reviews specs → architecture → layout → breakdown before anything is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowMode {
    /// Skip writing frozen tests after the stage-breakdown phase.
    pub skip_tests: bool,
    /// Stop the pipeline after this phase is approved (inclusive). `None` runs to the end.
    pub stop_after: Option<Phase>,
}

impl WorkflowMode {
    /// The full six-phase TDD pipeline: writes frozen tests, runs to work-decomposition.
    pub fn full_tdd() -> Self {
        Self {
            skip_tests: false,
            stop_after: None,
        }
    }

    /// Structured design only: run through the stage breakdown, write no tests, stop before
    /// decomposition/build. The Execute-plan flow — produce a reviewable plan, don't build yet.
    pub fn plan_only() -> Self {
        Self {
            skip_tests: true,
            stop_after: Some(Phase::StageBreakdown),
        }
    }
}

impl Default for WorkflowMode {
    fn default() -> Self {
        Self::full_tdd()
    }
}

/// What a completed workflow yields.
#[derive(Debug)]
pub struct WorkflowOutcome {
    /// The full artifact chain (all approved), persisted under the plan dir.
    pub state: WorkflowState,
    /// The subtask board parsed from the final (work-decomposition) phase — the
    /// swarm's input. Empty board if the model produced nothing parseable.
    pub board: TaskBoard,
    /// The test files the workers wrote from the Phase-4 coverage plan, already
    /// persisted to `workspace`. These are the frozen contract the implementation
    /// swarm must satisfy.
    pub test_files: Vec<String>,
    /// Whether the human aborted at a checkpoint. When true the pipeline stopped
    /// early: `state` holds the approved-so-far artifacts (kept, per spec 09) and
    /// `board`/`test_files` reflect only what was reached.
    pub aborted: bool,
}
