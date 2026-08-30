//! The integration point: a [`Solver`] is the thing being evaluated. It takes a
//! red workspace and tries to make it green.
//!
//! [`AgentSolver`] wraps the real `sc_core` agent loop driving a
//! `sc_model::ModelBackend`. The simpler [`FileSolver`]/[`NoopSolver`] keep the
//! harness testable without a model.

use std::cell::Cell;
use std::path::Path;

use sc_core::{run_agent, AgentConfig, ToolCallMetrics};
use sc_model::ModelBackend;
use sc_proto::{DcError, Result};

use crate::fsutil::copy_dir_recursive;
use crate::task::EvalTask;

/// Something that attempts to turn a red workspace green.
pub trait Solver {
    /// Identifier for reports (e.g. `"file-solver"`, or a future `"agent/e4b"`).
    fn name(&self) -> &str;
    /// Apply changes into `workspace` to satisfy `task`. The harness scores the
    /// result; a solver should not run the verification itself.
    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()>;
    /// Tool-call validity metrics from the most recent [`Solver::solve`], if the
    /// solver is model-driven. The runner reports these so the suite measures the
    /// M1 ≥95% valid-call target on a *real* backend (spec 07). Non-agent solvers
    /// have nothing to report.
    fn last_metrics(&self) -> Option<ToolCallMetrics> {
        None
    }

    /// Why the most recent [`Solver::solve`] ended, when the solver is model-driven.
    ///
    /// **A bare STILL-RED is not diagnosable.** It reads the same whether the model
    /// ran out of steps, stopped early believing it was done, or edited confidently
    /// and got the logic wrong -- three different problems with three different
    /// fixes. The agent already computes all of this; the solver used to discard
    /// everything but the call counts.
    fn last_run(&self) -> Option<RunInfo> {
        None
    }
}

/// The diagnostic tail of a model-driven solve.
#[derive(Debug, Clone)]
pub struct RunInfo {
    /// Model turns taken, against the step cap.
    pub steps: usize,
    /// Why the loop stopped, rendered.
    pub stop_reason: String,
    /// Whether the agent's OWN verification was green when it stopped. `None` when
    /// no verify command was configured.
    ///
    /// Worth reporting next to the harness's verdict: agent-green plus harness-red
    /// means the two disagree, which is a harness bug (a stale workspace, a
    /// different command) far more often than a model that lied.
    pub self_verified: Option<bool>,
    /// How many times the harness intervened to recover the run.
    pub interventions: usize,
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

/// The agent settings a task run actually needs, layered over `base`.
///
/// **`AgentConfig::default()` is tuned for toy tasks and was proven wrong against
/// real ones four separate times.** Task runs used it anyway -- `with_config`
/// existed and nothing called it -- so every measurement made on the SWE-bench path
/// was silently absent here. The numbers below are that path's, and each was paid
/// for:
///
/// * **steps 25 -> 40.** Pooled across every run, solves landed at step 33, 35 and
///   48. A 25-step cap throws those away and reports them as failures.
/// * **reserve 1024 -> 12288.** A reasoning model spends tokens thinking before it
///   emits the call, and a truncated turn yields NO call -- indistinguishable from
///   declining to act. At 1024 one edit request in five returned nothing at all.
/// * **observation cap 40 -> 200.** Forty lines amputates a pytest traceback right
///   where the assertion is.
/// * **read cap 400 -> 800.** The model must read the failing test; clipping it
///   mid-file is the harness hiding the answer.
///
/// It also fixes something worse than a bad number: `verify_command` was `None`, so
/// **the agent could not run the task's tests at all.** It edited blind and the
/// harness graded it afterwards. `contract_tests` become `frozen_paths` for the same
/// reason the SWE-bench path freezes test files -- the cheapest way to pass is to
/// edit the test.
fn task_config(base: AgentConfig, task: &EvalTask) -> AgentConfig {
    AgentConfig {
        max_steps: 40,
        response_reserve_tokens: 12288,
        observation_line_cap: 200,
        read_file_line_cap: 800,
        // The agent can finally check its own work. Without this it edits blind.
        verify_command: Some(task.verify_cmd.clone()),
        permission: sc_tools::PermissionPolicy {
            // Shell is what turns a SYMPTOM into a DIAGNOSIS: offered read+edit only,
            // one measured model picks read 15/16 and edits 1/16; with shell it picks
            // shell 14/16, with targeted greps, and the edit lands the turn after the
            // output makes the bug concrete. The false-pass risk (a model
            // `git checkout`-ing its way to green) is handled where it belongs -- the
            // harness re-verifies after the solve.
            allow_shell: true,
            // A solver that edits the contract test has not solved anything.
            frozen_paths: task.contract_tests.clone(),
            ..base.permission.clone()
        },
        ..base
    }
}

/// The real solver: runs the `sc_core` agent loop, driven by a model backend, to
/// turn the red workspace green. This is what scores an actual model on the suite.
pub struct AgentSolver<'a> {
    backend: &'a dyn ModelBackend,
    cfg: AgentConfig,
    /// Metrics from the most recent solve, for the runner to report.
    last: Cell<Option<ToolCallMetrics>>,
    /// Why the most recent solve ended -- the difference between a diagnosable
    /// failure and a bare STILL-RED. `RefCell` rather than `Cell` because
    /// `RunInfo` is not `Copy`.
    last_run: std::cell::RefCell<Option<RunInfo>>,
}

impl<'a> AgentSolver<'a> {
    pub fn new(backend: &'a dyn ModelBackend) -> Self {
        Self {
            backend,
            cfg: AgentConfig::default(),
            last: Cell::new(None),
            last_run: std::cell::RefCell::new(None),
        }
    }

    pub fn with_config(backend: &'a dyn ModelBackend, cfg: AgentConfig) -> Self {
        Self {
            backend,
            cfg,
            last: Cell::new(None),
            last_run: std::cell::RefCell::new(None),
        }
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
        // Per-task settings layered over whatever this solver was built with, so a
        // caller that passed an explicit config (verbosity, a sandbox) keeps it while
        // the task still supplies its own verify command and frozen tests.
        let cfg = task_config(self.cfg.clone(), task);
        // Backend errors (e.g. model unavailable) surface as a SolverError outcome.
        let report = run_agent(self.backend, &instruction, workspace, &cfg)?;
        self.last.set(Some(report.metrics));
        self.last_run.replace(Some(RunInfo {
            steps: report.steps,
            stop_reason: format!("{:?}", report.stop_reason),
            self_verified: report.verified,
            interventions: report.interventions,
        }));
        Ok(())
    }

    fn last_metrics(&self) -> Option<ToolCallMetrics> {
        self.last.get()
    }

    fn last_run(&self) -> Option<RunInfo> {
        self.last_run.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::TempWorkspace;
    use crate::runner::run_task;
    use sc_model::MockBackend;
    use serde_json::json;

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
            tags: Vec::new(),
            timeout_secs: None,
        };

        // Script the "model": write the correct impl, then finish.
        let backend = MockBackend::new([
            json!({
                "tool": "write_file",
                "path": "impl.sh",
                "content": "is_even() { [ $(( $1 % 2 )) -eq 0 ]; }\n"
            })
            .to_string(),
            json!({"tool": "finish"}).to_string(),
        ]);

        let solver = AgentSolver::new(&backend);
        let result = run_task(&task, &solver);
        assert!(
            result.outcome.is_pass(),
            "expected Pass, got {:?}",
            result.outcome
        );
    }

    /// The contract test is frozen at the TOOL layer, so the edit never lands.
    ///
    /// This is the first of two independent defences. It used to be the only place
    /// tampering was checked and it was checked only after the fact -- the write
    /// succeeded and the harness noticed afterwards. Denying it up front is better,
    /// but it is not sufficient on its own: `allow_shell` is on, so a model can still
    /// reach the file through a command. The post-hoc backstop below covers that,
    /// and the two are asserted separately so neither can quietly stop working.
    #[test]
    fn agent_solver_is_denied_when_it_tries_to_edit_the_test() {
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
            tags: Vec::new(),
            timeout_secs: None,
        };

        let backend = MockBackend::new([
            json!({"tool": "write_file", "path": "test.sh", "content": "exit 0\n"}).to_string(),
            json!({"tool": "finish"}).to_string(),
        ]);

        let result = run_task(&task, &AgentSolver::new(&backend));
        // Denied at the tool, so the run cannot reach a green -- and crucially it is
        // NOT scored as solved. The exact non-green outcome is not the point; that
        // the cheat did not work is.
        assert_ne!(
            result.outcome.symbol(),
            "PASS",
            "editing the contract test must never score as solved"
        );
        // And the file is untouched, which is the actual guarantee.
        let after = std::fs::read_to_string(fixture.path().join("test.sh")).unwrap();
        assert!(
            after.contains("is_even 4"),
            "the frozen contract test must survive the run, got: {after:?}"
        );
    }

    /// The post-hoc backstop: tampering that gets PAST the tool layer is still caught.
    ///
    /// `frozen_paths` denies the edit tools, but `allow_shell` is on -- deliberately,
    /// because shell is what turns a symptom into a diagnosis -- so a model can still
    /// reach the contract test through a command. Driving that path directly (rather
    /// than through a denied tool) keeps the hash check honest: without it, a suite
    /// could report a clean pass on a rewritten test.
    #[test]
    fn tampering_that_evades_the_tool_layer_is_still_caught() {
        let fixture = TempWorkspace::new("agent-tamper").unwrap();
        std::fs::write(
            fixture.path().join("impl.sh"),
            "is_even() { return 1; }
",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("test.sh"),
            ". ./impl.sh
is_even 4 || exit 1
exit 0
",
        )
        .unwrap();
        let task = EvalTask {
            id: "even".into(),
            description: "cheater".into(),
            fixture: fixture.path().to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec!["test.sh".into()],
            solution: None,
            tags: Vec::new(),
            timeout_secs: None,
        };

        // A solver that overwrites the contract test outright, as a shell command
        // could. It "passes" its own verification and must still be scored TAMPER.
        let cheat = FnSolver::new("cheat", |_task, ws: &Path| {
            std::fs::write(
                ws.join("test.sh"),
                "exit 0
",
            )?;
            Ok(())
        });

        assert_eq!(run_task(&task, &cheat).outcome.symbol(), "TAMPER");
    }
}
