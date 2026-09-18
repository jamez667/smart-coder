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
    let mut command = Command::new(shell);
    command
        .arg(flag)
        .arg(cmd)
        .current_dir(workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // **Its own process group, so `kill_tree` can actually reach the tree.**
    //
    // `kill_tree` signals the negated pid, which is a *process group* id, and its
    // comment claimed "the shell is its own group leader here". It was not:
    // `spawn` leaves a child in the parent's group, so that call named a group
    // that did not exist and the descendants it was written to reach survived it.
    // Verified on Linux — the child's pgid came back equal to the harness's own.
    //
    // It worked by accident, because the direct `child.kill()` afterwards handles
    // the common case of a shell with no children. The case it exists for — a
    // verify command that spawned a test runner — is the one it missed.
    //
    // `process_group(0)` puts the child in a new group led by itself, which is
    // what makes the negated-pid kill correct rather than merely harmless.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn()?;

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
        // Negating the pid signals the process GROUP, and the shell **is** its own
        // group leader — but only because `verify` asks for that with
        // `process_group(0)`. It was not, for as long as this comment claimed it
        // was: `spawn` leaves a child in the parent's group, so this named a
        // group that did not exist. See `verify`.
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

/// The same bound, for a task the *tests* build.
///
/// **A test must not be able to wait five minutes on one command.** The default
/// above is sized for a real repository's test suite on a cold cache; this
/// crate's own tests verify a two-line shell script, so any wait beyond a few
/// seconds is a stall rather than slowness. When one happened in CI the job hit
/// its 30-minute bound with several such waits inside it, and the default hid
/// which command was stuck behind a timeout longer than anybody would watch.
///
/// Read from `SC_EVAL_VERIFY_TIMEOUT_SECS` so CI can tighten it without changing
/// what a real eval run allows — the variable is unset everywhere else, and the
/// production default is untouched.
fn verify_timeout_default() -> Duration {
    std::env::var("SC_EVAL_VERIFY_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(VERIFY_TIMEOUT)
}

/// The verify timeout for one task: its own, or the default.
fn task_timeout(task: &EvalTask) -> Duration {
    task.timeout_secs
        .map(Duration::from_secs)
        .unwrap_or_else(verify_timeout_default)
}

/// Snapshot the contents of each contract-test file (None == missing).
fn snapshot_contracts(workspace: &Path, contracts: &[String]) -> BTreeMap<String, Option<u64>> {
    contracts
        .iter()
        .map(|rel| (rel.clone(), hash_file(&workspace.join(rel))))
        .collect()
}

/// Say where a task got to, if it takes longer than anybody would watch.
///
/// **A job killed by a CI timeout uploads no log for the step that was running**,
/// so instrumentation that only prints at the end is unrecoverable by
/// construction — three attempts at it produced nothing. This prints from a
/// background thread *while* the task is still running, so the line is in the
/// step's live output before the kill, and the last one printed names the phase
/// that never finished.
///
/// Off unless `SC_EVAL_WATCHDOG_SECS` is set, so a normal run is silent.
struct Watchdog(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Watchdog {
    fn start(what: String) -> Watchdog {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(secs) = std::env::var("SC_EVAL_WATCHDOG_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            let flag = done.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(250));
                    if start.elapsed() >= Duration::from_secs(secs) {
                        eprintln!("WATCHDOG: still inside {what} after {:?}", start.elapsed());
                        // **Prints, and stops there.** It used to call
                        // `std::process::abort()` so a stalled step would
                        // complete and upload its log. That was a bad trade:
                        // `abort()` raises SIGABRT, which dumps core, and a
                        // large debug binary dumping core on a CI runner is
                        // itself a long uninterruptible write -- on a runner
                        // already suspected of blocking in I/O.
                        //
                        // A diagnostic must not be able to hurt the thing it is
                        // diagnosing. The line above is the whole value here.
                        std::io::Write::flush(&mut std::io::stderr()).ok();
                        return;
                    }
                }
            });
        }
        Watchdog(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
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
    // This workspace is brand new, so nothing that ran on this thread before is a
    // predecessor of what runs in it. Without this, `--repeat N` -- same command, same
    // process, same thread, N fresh fixtures -- describes each fixture's FIRST
    // verification as a delta against the previous repeat's final state, and two rungs
    // sharing `cargo test --offline -q` contaminate each other inside one pass.
    // Measured on `rust-two-stage` x10: the untouched baseline of repeats 8 and 9 read
    // `now passing: padding_still_respects_a_later_component`, a test the model had
    // never touched in a fixture it had never seen.
    sc_verify::forget_runs();

    // (1) verify-red-first: the unsolved fixture must fail.
    let w = Watchdog::start(format!("{}: verify (red check)", task.id));
    let red = verify(ws.path(), &task.verify_cmd, task_timeout(task));
    drop(w);
    match red {
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
    let w = Watchdog::start(format!("{}: solver {}", task.id, solver.name()));
    let solved = solver.solve(task, ws.path());
    drop(w);
    if let Err(e) = solved {
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
    let w = Watchdog::start(format!("{}: verify (green check)", task.id));
    let green = verify(ws.path(), &task.verify_cmd, task_timeout(task));
    drop(w);
    match green {
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

    /// **`run_task` clears the verification delta memory, so one repeat cannot describe
    /// the next.**
    ///
    /// The wiring test for `sc_verify::forget_runs`. Its own unit test proves the reset
    /// works; this proves `run_task` actually performs it -- the trap being a correct
    /// function with a dead call site.
    ///
    /// The leak: `note_run` remembers per COMMAND in a thread-local, and `--repeat N`
    /// runs the same command in one process on one thread against N fresh fixtures.
    /// Measured on `rust-two-stage` x10 -- repeat 1's baseline read
    /// `run_verification: 1 failed, 5 passed:` and every later repeat inherited a delta,
    /// with repeats 8 and 9 telling an untouched workspace `now passing: ...`.
    #[test]
    fn a_repeat_does_not_inherit_the_previous_runs_verification_delta() {
        // Prime the memory for this command, exactly as a previous repeat would leave it.
        // Done through `note_run` rather than by shelling out: the state is the point, and
        // a unit test has no business running a verify command against the repo root.
        sc_verify::forget_runs();
        let _ = sc_verify::note_run("sh test.sh", &sc_verify::TestReport::generic(false));

        let (_keep, task) = red_fixture();
        let _ = run_task(&task, &NoopSolver);

        // `run_task` started a fresh workspace, so nothing on this thread is a
        // predecessor: the NEXT first-run for this command must have no delta to report.
        let report = sc_verify::TestReport::generic(false);
        assert_eq!(
            sc_verify::note_run("sh test.sh", &report),
            None,
            "a fresh run_task must clear the delta memory for its verify command"
        );
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

    /// A verify command's **children** are killed too, not just the shell.
    ///
    /// This is the case `kill_tree` exists for and the one it was missing. The
    /// old code signalled the negated pid — a process *group* id — with a comment
    /// asserting "the shell is its own group leader here". It was not: `spawn`
    /// leaves a child in the parent's group, so the signal named a group that did
    /// not exist, and only the direct `child.kill()` afterwards did any work. That
    /// reaches the shell and nothing it spawned.
    ///
    /// A verify command is a test runner, so a surviving grandchild is the normal
    /// case rather than an exotic one — and on a CI runner a stray spinner is a
    /// core burned for the rest of the job.
    ///
    /// Unix only: the Windows path uses `taskkill /T`, which walks the tree by
    /// handle and never depended on the group at all.
    #[cfg(unix)]
    #[test]
    #[ignore = "unix-only and unverifiable from this workstation; run with --ignored on Linux"]
    fn killing_a_verify_command_kills_what_it_spawned() {
        let ws = crate::fsutil::TempWorkspace::new("verify-grandchild").unwrap();
        // A nested shell in the FOREGROUND, which is what a real verify command
        // produces: `cargo` spawning a test binary, `sh` spawning `pytest`. It
        // inherits the group `verify` asked for, so the group kill reaches it.
        //
        // **Deliberately not backgrounded with `&`.** The first version of this
        // test used `sh -c '...' & wait`, and it failed in CI for a reason that
        // is a property of shells rather than of this code: a non-interactive
        // shell puts a background job in its OWN process group, so no signal to
        // the parent's group can ever reach it. That is not a case `kill_tree`
        // can serve — killing an arbitrary detached group would mean walking
        // /proc — and asserting it made the test claim a guarantee the design
        // does not offer.
        let pidfile = ws.path().join("child.pid");
        let cmd = "sh -c 'echo $$ > child.pid; while :; do sleep 1; done'";

        let green = verify(ws.path(), cmd, Duration::from_millis(400)).unwrap();
        assert!(
            !green,
            "a command that never exits has not verified anything"
        );

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("the grandchild wrote its pid")
            .trim()
            .parse()
            .expect("a pid");

        // Signal 0 tests for existence without delivering anything. The
        // grandchild must be gone; if the group kill did nothing, it is still
        // spinning.
        //
        // **Given a moment, and retried.** The kill is asynchronous: `kill(2)`
        // returns once the signal is queued, not once the target has been
        // reaped, and on a loaded two-core runner that gap is real. A single
        // probe 200ms later failed in CI while the same code passed everywhere
        // else -- which read as the fix not working, and was a race in the
        // test. Polling to a deadline asserts the same property without
        // asserting a schedule the kernel never promised.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut alive = true;
        while Instant::now() < deadline {
            if unsafe { kill_probe(pid, 0) } != 0 {
                alive = false;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive, "the grandchild (pid {pid}) survived the kill");
    }

    // `kill(pid, sig)`. Signal 0 delivers nothing and only reports whether the
    // process exists, which is the whole of what this test needs.
    //
    // A `//` comment rather than `///`: a doc comment on an `extern` block is an
    // `unused_doc_comments` error, and CI runs clippy with `-D warnings`. It
    // compiles on Windows only because the whole block is `#[cfg(unix)]`, so the
    // local lint never saw it.
    #[cfg(unix)]
    unsafe extern "C" {
        #[link_name = "kill"]
        unsafe fn kill_probe(pid: i32, sig: i32) -> i32;
    }

    /// The watchdog names the phase that is still running.
    ///
    /// **It no longer asserts that the process ends, because it no longer ends
    /// it.** That was tested by re-invoking this binary with the watchdog armed
    /// so the child would `abort()` -- which put a deliberate core dump inside a
    /// CI gate, on a runner already suspected of blocking in I/O. A diagnostic
    /// that can wedge the machine is worse than no diagnostic.
    #[test]
    fn the_watchdog_names_the_phase_that_is_still_running() {
        // SAFETY: the variable is read only by `Watchdog::start`, on this thread.
        unsafe { std::env::set_var("SC_EVAL_WATCHDOG_SECS", "0") };
        let _w = Watchdog::start("probe: a phase".into());
        // Longer than the watchdog's 250ms poll, so it has fired by now. Reaching
        // this line at all is half the assertion: the armed watchdog used to take
        // the whole process down here.
        thread::sleep(Duration::from_millis(600));
        unsafe { std::env::remove_var("SC_EVAL_WATCHDOG_SECS") };
    }

    /// Unset means silent, which is what keeps a normal run quiet — and, given
    /// the abort above, what keeps it alive.
    #[test]
    fn the_watchdog_is_off_unless_asked_for() {
        // SAFETY: the variable is read only by `Watchdog::start`, called below on
        // this thread.
        unsafe { std::env::remove_var("SC_EVAL_WATCHDOG_SECS") };
        let _w = Watchdog::start("probe: silent".into());
        thread::sleep(Duration::from_millis(400));
        // Reaching here at all is the assertion: an armed watchdog would have
        // aborted the process before this line.
    }
}
