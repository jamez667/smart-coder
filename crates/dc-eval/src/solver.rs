//! The integration point: a [`Solver`] is the thing being evaluated. It takes a
//! red workspace and tries to make it green.
//!
//! [`AgentSolver`] wraps the real `dc_core` agent loop driving a
//! `dc_model::ModelBackend` (the Android-core target, or a stand-in). The simpler
//! [`FileSolver`]/[`NoopSolver`] keep the harness testable without a model.

use std::path::Path;

use dc_core::{run_agent, AgentConfig};
use dc_model::ModelBackend;
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

/// The real solver: runs the `dc_core` agent loop, driven by a model backend, to
/// turn the red workspace green. This is what scores an actual model on the suite
/// (on-device via the Android-core `CallbackBackend`, or a stand-in off-device).
pub struct AgentSolver<'a> {
    backend: &'a dyn ModelBackend,
    cfg: AgentConfig,
}

impl<'a> AgentSolver<'a> {
    pub fn new(backend: &'a dyn ModelBackend) -> Self {
        Self {
            backend,
            cfg: AgentConfig::default(),
        }
    }

    pub fn with_config(backend: &'a dyn ModelBackend, cfg: AgentConfig) -> Self {
        Self { backend, cfg }
    }
}

impl Solver for AgentSolver<'_> {
    fn name(&self) -> &str {
        "agent"
    }

    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()> {
        // Ground the model in the goal and how it'll be checked. The harness — not
        // the agent — runs the actual verification afterwards.
        let instruction = format!(
            "Task: {}\n\nThe change is verified by running: {}\n\
             Make that command exit 0. Do not edit any test files.",
            task.description, task.verify_cmd
        );
        // Backend errors (e.g. model unavailable) surface as a SolverError outcome.
        run_agent(self.backend, &instruction, workspace, &self.cfg)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::TempWorkspace;
    use crate::runner::run_task;
    use dc_core::Tool;
    use dc_model::MockBackend;

    /// A model that drives the even-parity task to green purely via tool calls,
    /// then finishes. This is the full pipeline: model output -> tool calls ->
    /// file edits -> harness scores red->green, with no device required.
    #[test]
    fn agent_solver_drives_even_parity_to_green() {
        // Build the red fixture.
        let fixture = TempWorkspace::new("agent-fixture").unwrap();
        std::fs::write(fixture.path().join("impl.sh"), "is_even() { return 1; }\n").unwrap();
        std::fs::write(
            fixture.path().join("test.sh"),
            ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
        )
        .unwrap();
        let task = EvalTask {
            id: "even".into(),
            description: "Fix is_even so even numbers are reported even.".into(),
            fixture: fixture.path().to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec!["test.sh".into()],
            solution: None,
        };

        // Script the "model": write the correct impl, then finish.
        let fix = Tool::WriteFile {
            path: "impl.sh".into(),
            content: "is_even() { [ $(( $1 % 2 )) -eq 0 ]; }\n".into(),
        };
        let backend = MockBackend::new([
            serde_json::to_string(&fix).unwrap(),
            serde_json::to_string(&Tool::Finish).unwrap(),
        ]);

        let solver = AgentSolver::new(&backend);
        let result = run_task(&task, &solver);
        assert!(
            result.outcome.is_pass(),
            "expected Pass, got {:?}",
            result.outcome
        );
    }

    /// If the model cheats by rewriting the contract test, the harness must catch
    /// it as tampering even though the suite would then "pass".
    #[test]
    fn agent_solver_cannot_cheat_by_editing_the_test() {
        let fixture = TempWorkspace::new("agent-cheat").unwrap();
        std::fs::write(fixture.path().join("impl.sh"), "is_even() { return 1; }\n").unwrap();
        std::fs::write(
            fixture.path().join("test.sh"),
            ". ./impl.sh\nis_even 4 || exit 1\nexit 0\n",
        )
        .unwrap();
        let task = EvalTask {
            id: "even".into(),
            description: "cheater".into(),
            fixture: fixture.path().to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec!["test.sh".into()],
            solution: None,
        };

        let cheat = Tool::WriteFile {
            path: "test.sh".into(),
            content: "exit 0\n".into(),
        };
        let backend = MockBackend::new([
            serde_json::to_string(&cheat).unwrap(),
            serde_json::to_string(&Tool::Finish).unwrap(),
        ]);

        let result = run_task(&task, &AgentSolver::new(&backend));
        assert_eq!(result.outcome.symbol(), "TAMPER");
    }
}
