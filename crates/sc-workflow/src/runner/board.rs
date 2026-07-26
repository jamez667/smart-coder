//! The decomposition artifact → the swarm's [`TaskBoard`] (spec 09 → spec 08).

use sc_swarm::{parse_subtasks_on_stack, TaskBoard};

use crate::phase::Phase;
use crate::stack::ProjectStack;
use crate::state::WorkflowState;

pub(super) fn artifact_content(state: &WorkflowState, phase: Phase) -> String {
    state
        .artifact(phase)
        .map(|a| a.content.clone())
        .unwrap_or_default()
}

/// Parse the work-decomposition artifact into the swarm's [`TaskBoard`] (spec 09
/// → spec 08 input contract), then drop any non-implementation subtasks the model
/// slipped in: ones that target a frozen test file, or that name no files at all
/// (e.g. a "run the tests" step — the harness verifies, that's not worker work).
/// Tests are already written and frozen; the swarm only implements.
pub(super) fn board_from(
    state: &WorkflowState,
    test_files: &[String],
    stack: ProjectStack,
) -> TaskBoard {
    let is_test = |f: &str| {
        let n = f.replace('\\', "/");
        test_files.iter().any(|t| t.replace('\\', "/") == n)
    };
    // Parse against THIS project's stack, so the drift filter keeps its own files (the bug: the
    // filter was hardcoded to Python and dropped every .rs subtask → empty board → nothing built).
    let subtasks: Vec<_> = state
        .artifact(Phase::WorkDecomposition)
        .map(|a| parse_subtasks_on_stack(&a.content, stack.on_stack_exts()))
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.files.is_empty() && !s.files.iter().all(|f| is_test(f)))
        .collect();
    TaskBoard::new(subtasks)
}
