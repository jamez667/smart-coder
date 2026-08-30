//! The scoring loop. For each task the runner enforces the TDD invariants from
//! spec 11:
//!
//! 1. **verify-red-first** — the fixture's test must *fail* before solving, else
//!    the test is vacuous.
//! 2. **frozen contract tests** — the solver must not modify any declared
//!    contract-test file.
//! 3. **green after solve** — the test must pass once the solver is done.
//! 4. (implicit) the whole `verify_cmd` is the gate, so breaking anything it
//!    checks counts as failure.
//!
//! The runner never panics: every failure mode is a returned [`Outcome`].

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::fsutil::{copy_dir_recursive, hash_file, TempWorkspace};
use crate::solver::Solver;
use crate::task::EvalTask;

/// The graded result of one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Red before, green after, contract intact. Success.
    Pass,
    /// The fixture was already green before solving — the test proves nothing.
    NotRedFirst,
    /// A contract-test file was modified (or deleted) by the solver.
    ContractTampered(String),
    /// After solving, verification still fails.
    StillRed,
    /// The solver returned an error.
    SolverError(String),
    /// The harness itself failed (workspace setup, spawning the verifier, ...).
    HarnessError(String),
}

impl Outcome {
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass)
    }

    /// Short symbol for reports.
    pub fn symbol(&self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::NotRedFirst => "NOT-RED",
            Outcome::ContractTampered(_) => "TAMPER",
            Outcome::StillRed => "STILL-RED",
            Outcome::SolverError(_) => "SOLVER-ERR",
            Outcome::HarnessError(_) => "HARNESS-ERR",
        }
    }
}

/// One task's id paired with its outcome.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub id: String,
    pub solver: String,
    pub outcome: Outcome,
    /// Tool-call validity metrics, when the solver is model-driven (spec 07).
    pub metrics: Option<sc_core::ToolCallMetrics>,
    /// Why a model-driven solve ended. Reported on failures, where "STILL-RED" on
    /// its own cannot distinguish running out of steps from stopping early from
    /// simply being wrong.
    pub run: Option<crate::solver::RunInfo>,
}

/// Run `verify_cmd` inside `workspace`. `Ok(true)` == exit 0 == green.
///
/// The verifier's stdout/stderr is captured (not inherited) so the harness's own
/// report isn't polluted by the *intentional* red-first failures.
fn verify(workspace: &Path, cmd: &str, timeout: Duration) -> std::io::Result<bool> {
    // `sh` was hardcoded here, so on Windows -- the machine this is developed on --
    // verification depended on a POSIX shell being on PATH. When it was not, every
    // task scored as failing to go green, which is indistinguishable from a solver
    // that produced nothing. `sc_verify::build_command` already branches on the
    // platform, so use the same shell selection the rest of the harness does.
    // Shell selection lives in `sc_verify` -- one decision, one place. It was
    // duplicated here first, which is how the agent's own `run_command` kept using
    // `cmd` after this function had been fixed.
    let (shell, flag) = sc_verify::host_shell();
    let mut child = Command::new(shell)
        .arg(flag)
        .arg(cmd)
        .current_dir(workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Bounded, because the thing being verified is code a MODEL wrote. An infinite
    // loop in a solution used to hang the whole suite with no output -- indefinitely,
    // in CI. A timeout is a failed verification, not a crash: the task scores red,
    // which is the honest answer.
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Kill a timed-out verify command *and everything it spawned*.
///
/// `Child::kill` only kills the shell. The shell's own children survive it, and
/// since they inherit its handles, the follow-up `wait()` then blocks on them --
/// so the naive "kill then wait" pair hangs on exactly the input it exists to
/// handle. Found the honest way: this function's own test spun forever and left an
/// orphaned `PING` running after the harness had supposedly killed it.
///
/// A verify command is a test runner, so it almost always has children (a `cargo`
/// that spawned a test binary, a `sh` that spawned `pytest`). Killing the tree is
/// the normal case here, not an edge case.
fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id();

    #[cfg(windows)]
    {
        // `/T` is the whole point: terminate this pid and every descendant.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(unix)]
    {
        // Negating the pid signals the process GROUP. The shell is its own group
        // leader here, so this reaches the children it spawned.
        let _ = Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // Direct kill too, in case the platform helper is unavailable, then reap. By now
    // the descendants are gone, so this cannot block the way the old pair did.
    let _ = child.kill();
    let _ = child.wait();
}

/// How long a task's verify command may run before it is treated as failed.
///
/// Generous, because a real test suite on a cold cargo cache is slow; the point is
/// only to bound a solution that never terminates.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(300);

/// The verify timeout for one task: its own, or the default.
fn task_timeout(task: &EvalTask) -> Duration {
    task.timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(VERIFY_TIMEOUT)
}

/// Snapshot the contents of each contract-test file (None == missing).
fn snapshot_contracts(workspace: &Path, contracts: &[String]) -> BTreeMap<String, Option<u64>> {
    contracts
        .iter()
        .map(|rel| (rel.clone(), hash_file(&workspace.join(rel))))
        .collect()
}

/// Score a single task against a solver. Always returns a [`TaskResult`].
pub fn run_task(task: &EvalTask, solver: &dyn Solver) -> TaskResult {
    let result = |outcome| TaskResult {
        id: task.id.clone(),
        solver: solver.name().to_string(),
        outcome,
        metrics: None,
        run: None,
    };
    // Like `result`, but attaches the solver's tool-call metrics (post-solve).
    let result_with_metrics = |outcome| TaskResult {
        id: task.id.clone(),
        solver: solver.name().to_string(),
        outcome,
        metrics: solver.last_metrics(),
        run: solver.last_run(),
    };

    // Materialize the fixture into an isolated, self-cleaning workspace.
    let ws = match TempWorkspace::new(&task.id) {
        Ok(ws) => ws,
        Err(e) => return result(Outcome::HarnessError(format!("temp workspace: {e}"))),
    };
    if let Err(e) = copy_dir_recursive(&task.fixture, ws.path()) {
        return result(Outcome::HarnessError(format!(
            "copying fixture {}: {e}",
            task.fixture.display()
        )));
    }

    // (1) verify-red-first: the unsolved fixture must fail.
    match verify(ws.path(), &task.verify_cmd, task_timeout(task)) {
        Ok(true) => return result(Outcome::NotRedFirst),
        Ok(false) => {}
        Err(e) => {
            return result(Outcome::HarnessError(format!(
                "running verifier (red check): {e}"
            )))
        }
    }

    // Snapshot contract tests before handing the workspace to the solver.
    let before = snapshot_contracts(ws.path(), &task.contract_tests);

    // Let the solver attempt the task.
    if let Err(e) = solver.solve(task, ws.path()) {
        return result(Outcome::SolverError(e.to_string()));
    }

    // (2) frozen contract tests: nothing the solver touched may differ.
    let after = snapshot_contracts(ws.path(), &task.contract_tests);
    for (path, before_hash) in &before {
        if after.get(path) != Some(before_hash) {
            return result_with_metrics(Outcome::ContractTampered(path.clone()));
        }
    }

    // (3) green after solve.
    match verify(ws.path(), &task.verify_cmd, task_timeout(task)) {
        Ok(true) => result_with_metrics(Outcome::Pass),
        Ok(false) => result_with_metrics(Outcome::StillRed),
        Err(e) => result_with_metrics(Outcome::HarnessError(format!(
            "running verifier (green check): {e}"
        ))),
    }
}

/// Score every task in a suite against the same solver.
pub fn run_suite(tasks: &[EvalTask], solver: &dyn Solver) -> Vec<TaskResult> {
    tasks.iter().map(|t| run_task(t, solver)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solver::{FnSolver, NoopSolver};
    use std::fs;
    use std::path::PathBuf;

    /// Build a red fixture: `impl.sh` is a wrong stub, `test.sh` is the contract.
    fn red_fixture() -> (TempWorkspace, EvalTask) {
        let dir = TempWorkspace::new("fixture").unwrap();
        fs::write(
            dir.path().join("impl.sh"),
            "is_even() { return 1; }\n", // always "odd" -> red
        )
        .unwrap();
        fs::write(
            dir.path().join("test.sh"),
            ". ./impl.sh\n\
             is_even 4 || exit 1\n\
             if is_even 3; then exit 1; fi\n\
             exit 0\n",
        )
        .unwrap();
        let task = EvalTask {
            id: "even".into(),
            description: "fix is_even".into(),
            fixture: dir.path().to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec!["test.sh".into()],
            solution: None,
            tags: Vec::new(),
            timeout_secs: None,
        };
        (dir, task)
    }

    fn correct_impl() -> &'static str {
        "is_even() { [ $(( $1 % 2 )) -eq 0 ]; }\n"
    }

    #[test]
    fn passes_when_solver_makes_it_green() {
        let (_keep, task) = red_fixture();
        let solver = FnSolver::new("good", |_t, ws: &std::path::Path| {
            fs::write(ws.join("impl.sh"), correct_impl())?;
            Ok(())
        });
        assert_eq!(run_task(&task, &solver).outcome, Outcome::Pass);
    }

    #[test]
    fn still_red_when_solver_does_nothing() {
        let (_keep, task) = red_fixture();
        assert_eq!(run_task(&task, &NoopSolver).outcome, Outcome::StillRed);
    }

    #[test]
    fn detects_contract_tampering_even_if_tests_pass() {
        let (_keep, task) = red_fixture();
        // This "solver" cheats: it rewrites the contract test to always pass.
        let solver = FnSolver::new("cheater", |_t, ws: &std::path::Path| {
            fs::write(ws.join("impl.sh"), correct_impl())?;
            fs::write(ws.join("test.sh"), "exit 0\n")?; // tamper!
            Ok(())
        });
        match run_task(&task, &solver).outcome {
            Outcome::ContractTampered(p) => assert_eq!(p, "test.sh"),
            other => panic!("expected ContractTampered, got {other:?}"),
        }
    }

    #[test]
    fn rejects_vacuous_test_that_passes_before_solving() {
        let dir = TempWorkspace::new("vacuous").unwrap();
        fs::write(dir.path().join("test.sh"), "exit 0\n").unwrap(); // green from the start
        let task = EvalTask {
            id: "vacuous".into(),
            description: "vacuous".into(),
            fixture: dir.path().to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec![],
            solution: None,
            tags: Vec::new(),
            timeout_secs: None,
        };
        assert_eq!(run_task(&task, &NoopSolver).outcome, Outcome::NotRedFirst);
    }

    #[test]
    fn surfaces_solver_errors() {
        let (_keep, task) = red_fixture();
        let solver = FnSolver::new("boom", |_t, _ws: &std::path::Path| {
            Err(sc_proto::DcError::Eval("boom".into()))
        });
        match run_task(&task, &solver).outcome {
            Outcome::SolverError(m) => assert!(m.contains("boom")),
            other => panic!("expected SolverError, got {other:?}"),
        }
    }

    #[test]
    fn harness_error_when_fixture_missing() {
        let task = EvalTask {
            id: "missing".into(),
            description: "missing fixture".into(),
            fixture: PathBuf::from("/no/such/fixture/dir/xyzzy"),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec![],
            solution: None,
            tags: Vec::new(),
            timeout_secs: None,
        };
        assert!(matches!(
            run_task(&task, &NoopSolver).outcome,
            Outcome::HarnessError(_)
        ));
    }

    /// A verify command that never terminates must score red, not hang the suite.
    ///
    /// The thing being verified is code a MODEL wrote, so a non-terminating solution
    /// is a routine outcome rather than an exotic one. Unbounded `.output()` used to
    /// park the whole run here with no output at all.
    #[test]
    fn a_verify_command_that_never_ends_is_a_failure_not_a_hang() {
        let ws = crate::fsutil::TempWorkspace::new("verify-timeout").unwrap();
        // Portable spin: both `cmd` and `sh` understand an infinite loop via ping/sleep,
        // but the simplest cross-platform one is a busy conditional that never exits.
        let forever = if cfg!(windows) {
            "ping -t 127.0.0.1"
        } else {
            "while :; do :; done"
        };

        let start = Instant::now();
        let green = verify(ws.path(), forever, Duration::from_millis(300)).unwrap();
        let elapsed = start.elapsed();

        assert!(
            !green,
            "a command that never exits has not verified anything"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "should be killed at the deadline, took {elapsed:?}"
        );
    }
}
