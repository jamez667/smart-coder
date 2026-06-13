//! The integration point: a [`Solver`] is the thing being evaluated. It takes a
//! red workspace and tries to make it green.
//!
//! The real agent loop (M3+) will implement this using a `dc_model::ModelBackend`
//! (the Android-core target, or a stand-in). For M1 we ship simple solvers so the
//! harness itself is testable end-to-end before the agent exists.

use std::path::Path;

use dc_proto::{DcError, Result};

use crate::fsutil::copy_dir_recursive;
use crate::task::EvalTask;

/// Something that attempts to turn a red workspace green.
pub trait Solver {
    /// Identifier for reports (e.g. `"file-solver"`, or a future `"agent/e4b"`).
    fn name(&self) -> &str;
    /// Apply changes into `workspace` to satisfy `task`. The harness scores the
    /// result; a solver should not run the verification itself.
    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()>;
}

/// Applies a task's known-good `solution` directory over the workspace.
///
/// Used to (a) exercise the harness and (b) demonstrate a green run before the
/// real agent exists. It deliberately only copies the files in `solution/`, so a
/// well-formed task leaves contract tests untouched.
pub struct FileSolver;

impl Solver for FileSolver {
    fn name(&self) -> &str {
        "file-solver"
    }

    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()> {
        let solution = task.solution.as_ref().ok_or_else(|| {
            DcError::Eval(format!(
                "task '{}' has no `solution` for FileSolver",
                task.id
            ))
        })?;
        copy_dir_recursive(solution, workspace)
            .map_err(|e| DcError::Eval(format!("applying solution for '{}': {e}", task.id)))?;
        Ok(())
    }
}

/// A solver that does nothing — leaves the workspace red. Used to prove the
/// harness reports an unsolved task as a failure rather than a pass.
pub struct NoopSolver;

impl Solver for NoopSolver {
    fn name(&self) -> &str {
        "noop-solver"
    }

    fn solve(&self, _task: &EvalTask, _workspace: &Path) -> Result<()> {
        Ok(())
    }
}

/// Wrap a closure as a [`Solver`]. Handy for tests.
pub struct FnSolver<F> {
    name: String,
    f: F,
}

impl<F> FnSolver<F>
where
    F: Fn(&EvalTask, &Path) -> Result<()>,
{
    pub fn new(name: impl Into<String>, f: F) -> Self {
        Self {
            name: name.into(),
            f,
        }
    }
}

impl<F> Solver for FnSolver<F>
where
    F: Fn(&EvalTask, &Path) -> Result<()>,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()> {
        (self.f)(task, workspace)
    }
}
