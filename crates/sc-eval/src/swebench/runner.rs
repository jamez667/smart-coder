//! Running one SWE-bench instance end to end.

use std::time::Instant;

use sc_proto::{DcError, Result};
use serde::Serialize;

use super::container::{in_testbed, InstanceContainer, TESTBED};
use super::instance::SweInstance;
use super::score::SweScore;
use crate::fsutil::TempWorkspace;

/// **Load-bearing.** `-rA` prints one `PASSED <nodeid>` / `FAILED <nodeid>` line per
/// test, which is the only pytest format that names the *passes*. Scoring
/// PASS_TO_PASS needs them by name; a quieter flag makes every expected pass look
/// `missing` and every instance score unresolved. Do not "tidy" this to `-v` — that
/// puts the status last, and the parser reads these as a prefix.
pub const PYTEST_FLAGS: &str = "-rA --tb=line -p no:cacheprovider";

/// How long any single command inside the container may take.
const TIMEOUT_SECS: u64 = 900;

/// What happened to one instance.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceRun {
    pub instance_id: String,
    pub repo: String,
    /// SWE-bench resolution — see [`SweScore::resolved`].
    pub resolved: bool,
    pub score: SweScore,
    /// Set when the instance could not be scored at all (image missing, patch would
    /// not apply, never went red). Distinct from "the model failed": this is the
    /// harness failing, and it must not be averaged in as a miss.
    pub harness_error: Option<String>,
    /// The agent edited a file it was told not to.
    pub tampered: Option<String>,
    pub duration_ms: u128,
    pub steps: Option<usize>,
    pub stop_reason: Option<String>,
    pub tool_calls_valid: Option<usize>,
    pub tool_calls_invalid: Option<usize>,
}

impl InstanceRun {
    fn failed(instance: &SweInstance, why: String, started: Instant) -> InstanceRun {
        InstanceRun {
            instance_id: instance.instance_id.clone(),
            repo: instance.repo.clone(),
            resolved: false,
            score: SweScore::default(),
            harness_error: Some(why),
            tampered: None,
            duration_ms: started.elapsed().as_millis(),
            steps: None,
            stop_reason: None,
            tool_calls_valid: None,
            tool_calls_invalid: None,
        }
    }
}

/// The pytest command for a set of node ids: run their FILES, not the ids.
///
/// **This is what keeps the benchmark unmodified.** Passing ids directly puts every one
/// on the command line, and pytest ABORTS on a single id it cannot resolve — "no tests
/// ran" — so one bad argument zeroes the whole instance. Some upstream rows carry ids
/// that were split on whitespace, leaving fragments like
/// `test_locate_app[cliapp.factory-`, plus stray progress output like `[100%]`. Under
/// the id-passing form those made four instances unscoreable, which is what tempted an
/// earlier version of this harness to *filter the dataset*. That was the wrong fix: a
/// benchmark you have edited is no longer comparable to anyone else's run of it.
///
/// Running the files instead is what the official SWE-bench harness does. An id that
/// does not exist is then simply never seen in the output and scores as `missing`,
/// which is correct and local to that one test — every other id still scores normally,
/// and the vendored instances stay byte-for-byte as published.
///
/// Files are single-quoted (paths can contain shell metacharacters) and deduplicated:
/// 115 ids across two files is two arguments.
///
/// The cost is running the file's other tests too, which is a little slower and is why
/// [`PYTEST_FLAGS`] keeps `-rA` — scoring reads the named results out of the output and
/// ignores the rest.
pub fn pytest_command(tests: &[String]) -> String {
    let mut files: Vec<&str> = tests
        .iter()
        // An entry with no `::` is not a node id — upstream progress output like
        // `[100%]` — and naming it would put the junk back on the command line, which
        // is the abort this function exists to avoid. Skipping it here changes no
        // score: the id is still in the instance and still counts as `missing`.
        .filter(|t| t.contains("::"))
        .map(|t| t.split("::").next().unwrap_or(t))
        .filter(|f| !f.is_empty())
        .collect();
    files.sort_unstable();
    files.dedup();
    let args = files
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(" ");
    // `$SC_PY` is set by `Benchmark::python_prefix` — empty when the image's system
    // Python already has the project's dependencies, `poetry run` when it does not.
    // Unquoted so an empty value expands to nothing rather than an empty argument.
    format!("$SC_PY python -m pytest {PYTEST_FLAGS} {args}")
}

/// Solves an instance by editing the source tree at `workspace`.
pub trait SweSolver {
    fn name(&self) -> &str;
    /// Attempt the fix. `workspace` holds the source subtree plus the failing test
    /// files, which may be read but must not be edited.
    fn solve(&self, instance: &SweInstance, workspace: &std::path::Path) -> Result<()>;
    /// Diagnostics from the most recent solve, when the solver is model-driven.
    fn last_report(&self) -> Option<SolveReport> {
        None
    }
}

/// What a model-driven solver reports back about its own run.
#[derive(Debug, Clone, Default)]
pub struct SolveReport {
    pub steps: usize,
    pub stop_reason: String,
    pub tool_calls_valid: usize,
    pub tool_calls_invalid: usize,
}

/// Run one instance: start the image, apply the test patch, prove it red, solve, prove
/// the tests were not touched, then score.
pub fn run_instance(instance: &SweInstance, solver: &dyn SweSolver) -> InstanceRun {
    let started = Instant::now();
    match run_instance_inner(instance, solver, started) {
        Ok(r) => r,
        Err(e) => InstanceRun::failed(instance, e.to_string(), started),
    }
}

fn run_instance_inner(
    instance: &SweInstance,
    solver: &dyn SweSolver,
    started: Instant,
) -> Result<InstanceRun> {
    // The workspace comes first: the container is named after it, so that the agent's
    // own `run_verification` (which addresses a container by a hash of its workspace
    // path) reaches THIS container and runs the real test suite in the real
    // environment. See `InstanceContainer::session_name_for`.
    let ws = TempWorkspace::new(&instance.instance_id)
        .map_err(|e| DcError::Eval(format!("workspace: {e}")))?;
    let src = ws.path().join("src");
    std::fs::create_dir_all(&src).map_err(|e| DcError::Eval(format!("src dir: {e}")))?;
    let container = InstanceContainer::start_named(
        &InstanceContainer::session_name_for(&src),
        &instance.image(),
        ws.path(),
    )?;

    // 1. Apply the test patch. This is what turns the base commit red: the image ships
    //    the *old* tests, and the patch rewrites them to expect the fixed behaviour.
    let patch_path = ws.path().join("test.patch");
    std::fs::write(&patch_path, &instance.test_patch)
        .map_err(|e| DcError::Eval(format!("writing test patch: {e}")))?;
    container.copy_in(&patch_path, "/tmp/test.patch")?;
    //    Committed, not just applied: the freeze check below asks git what changed,
    //    and an uncommitted test patch would show up as the harness tampering with its
    //    own tests. Committing makes the patched tree the baseline the agent is
    //    measured against.
    let py = instance.benchmark.python_prefix();
    let (ok, out) = container.exec(&in_testbed(
        py,
        concat!(
            "git apply /tmp/test.patch && ",
            "git -c user.email=eval@sc -c user.name=sc-eval ",
            "commit -am 'test patch' --no-verify"
        ),
        120,
    ))?;
    if !ok {
        return Ok(InstanceRun::failed(
            instance,
            format!("test patch did not apply: {}", out.trim()),
            started,
        ));
    }

    // 2. Red check. An instance that is already green here is broken, not easy — and
    //    scoring it as solved would be the worst possible failure of this harness.
    let cmd = pytest_command(&instance.all_tests());
    let (_, out) = container.exec(&in_testbed(py, &cmd, TIMEOUT_SECS))?;
    let red = SweScore::grade(instance, &sc_verify::parse(&cmd, &out, false));
    if !red.is_red_start() {
        // Name the offending tests. A bare "not red at setup" cost hours of manual
        // bisecting to find out *which* test was wrong and whether it was our fault:
        // an F2P that already passes usually means the test patch did not take, while
        // a P2P already failing is nearly always the published image (dynaconf-1241
        // reads DOTENV_INT from a `.env` the image never shipped; llama_deploy-384
        // raises ModuleNotFoundError on an optional dependency).
        let mut why = format!("not red at setup ({})", red.line());
        if !red.f2p_passed.is_empty() {
            why.push_str(&format!(
                " — F2P already passing: {}",
                red.f2p_passed.join(", ")
            ));
        }
        if !red.p2p_broken.is_empty() {
            let shown: Vec<&str> = red.p2p_broken.iter().take(3).map(String::as_str).collect();
            why.push_str(&format!(
                " — P2P already failing: {}{}",
                shown.join(", "),
                if red.p2p_broken.len() > 3 {
                    format!(" (+{} more)", red.p2p_broken.len() - 3)
                } else {
                    String::new()
                }
            ));
        }
        return Ok(InstanceRun::failed(instance, why, started));
    }

    // 3. Hand the agent the source subtree AND the failing tests.
    //
    // The tests are read-only, not invisible. Withholding them (an earlier design
    // here, meant to make the freeze structural) makes the task close to
    // unsolvable: the model is asked to satisfy `test_unknown_option_name`, tries to
    // read it, is told the path does not exist, searches for it, gets no matches, and
    // is left guessing at the expected behaviour from the issue text alone. Traced on
    // qwen3-coder-30b: turn 7 read_file on the test -> "cannot find the path", turn 11
    // search_code -> "no matches", then it thrashed until the stall detector stopped
    // it. Every real SWE-bench scaffold shows the model the tests.
    //
    // Freezing is enforced instead by `PermissionPolicy::with_frozen` in the loop and
    // the `git diff` check below — the same belt-and-braces the original `run_task`
    // uses for its contract tests.
    // `copy_out` names where the entry LANDS, so the destination carries the leaf:
    // `src_dir` "pylint" lands at `<ws>/src/pylint`, "src/gitingest" at
    // `<ws>/src/gitingest`.
    let src_leaf = instance
        .src_dir
        .rsplit('/')
        .next()
        .unwrap_or(&instance.src_dir);
    container.copy_out(
        &format!("{TESTBED}/{}", instance.src_dir),
        &src.join(src_leaf),
    )?;
    for f in &instance.test_files {
        // Only the files the scored node ids name, not the whole test tree: pylint's
        // `tests/functional/s/symlink/` holds symlinks that need special handling on
        // Windows (see `InstanceContainer::copy_out`).
        //
        // Skip anything the source copy already brought over. Some repos keep their
        // tests INSIDE the source subtree — `kubernetes-client/python` has
        // `kubernetes/base/config/*_test.py` under `src_dir: kubernetes` — and copying
        // again fails with "Cannot create a file when that file already exists"
        // (os error 183), which failed the whole instance.
        let dest = src.join(f);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DcError::Eval(format!("test dir: {e}")))?;
        }
        container.copy_out(&format!("{TESTBED}/{f}"), &dest)?;
    }

    let solve_err = solver.solve(instance, &src).err();
    let report = solver.last_report();

    // 4. Copy the edited source back and re-score.
    let leaf = instance
        .src_dir
        .rsplit('/')
        .next()
        .unwrap_or(&instance.src_dir);
    let parent = match instance.src_dir.rsplit_once('/') {
        Some((p, _)) => format!("{TESTBED}/{p}"),
        None => TESTBED.to_string(),
    };
    // Streamed, not `docker cp`: see `InstanceContainer::copy_dir_in`.
    container.copy_dir_in(&src, leaf, &parent)?;

    // 5. Freeze check: did the agent change a test file?
    //
    //    Compared by CONTENT, not by git's index. In most repos the tests sit outside
    //    `src_dir` and never reach the host at all, but some keep them inside it
    //    (`cyclotruc/gitingest` has `src/gitingest/tests/`), so the copy-out/copy-back
    //    round-trips them. `git diff --name-only` alone then reports a file as modified
    //    on a stat change with identical bytes — measured: the NOOP solver, which
    //    writes nothing, was reported as tampering with `test_clone.py`, and a run
    //    where the model had legitimately passed every test scored unresolved.
    //
    //    `--stat` after `update-index --refresh` compares content, and
    //    `--ignore-all-space` absorbs the line-ending churn a Windows host adds on the
    //    way through. A real edit still shows up; a byte-identical round-trip does not.
    let paths = instance.test_files.join(" ");
    let (_, diff) = container.exec(&in_testbed(
        py,
        &format!(
            "git update-index -q --refresh -- {paths} >/dev/null 2>&1; \
             git diff --name-only --ignore-all-space -- {paths}"
        ),
        120,
    ))?;
    let tampered = diff
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string);

    // 6. Green check.
    let (_, out) = container.exec(&in_testbed(py, &cmd, TIMEOUT_SECS))?;
    let score = SweScore::grade(instance, &sc_verify::parse(&cmd, &out, false));

    Ok(InstanceRun {
        instance_id: instance.instance_id.clone(),
        repo: instance.repo.clone(),
        // Ids that were already unfindable at setup do not count against the model —
        // see `SweScore::resolved_excluding`. Anything that went missing DURING the
        // solve still does.
        resolved: score.resolved_excluding(&red.missing) && tampered.is_none(),
        score,
        harness_error: solve_err.map(|e| e.to_string()),
        tampered,
        duration_ms: started.elapsed().as_millis(),
        steps: report.as_ref().map(|r| r.steps),
        stop_reason: report.as_ref().map(|r| r.stop_reason.clone()),
        tool_calls_valid: report.as_ref().map(|r| r.tool_calls_valid),
        tool_calls_invalid: report.as_ref().map(|r| r.tool_calls_invalid),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pytest_command_keeps_the_flags_that_name_passes() {
        let c = pytest_command(&["t.py::a".into(), "t.py::b".into()]);
        // `-rA` is what makes the PASSES appear by name in the output, which is how
        // PASS_TO_PASS is scored once the ids are no longer on the command line.
        assert!(c.contains("-rA"), "-rA names the passes: {c}");
        assert!(c.contains("'t.py'"), "the file is quoted: {c}");
    }

    /// The whole point: a malformed id must not be able to abort the run.
    ///
    /// Upstream rows carry ids that were split on whitespace (`...factory-`) and stray
    /// progress output (`[100%]`). Passing ids directly, pytest aborts on the first one
    /// it cannot resolve and NOTHING runs. Passing files, the junk never reaches the
    /// command line and every real test still scores.
    #[test]
    fn malformed_ids_do_not_reach_the_command_line() {
        let c = pytest_command(&[
            "tests/test_cli.py::TestRoutes::test_host".into(),
            "tests/test_cli.py::test_locate_app[cliapp.factory-".into(),
            "[100%]".into(),
        ]);
        assert!(c.contains("'tests/test_cli.py'"), "{c}");
        assert!(!c.contains("factory-"), "no fragment on the line: {c}");
        assert!(!c.contains("100%"), "no progress output on the line: {c}");
    }

    /// One file, however many ids name it.
    #[test]
    fn the_files_are_deduplicated() {
        let c = pytest_command(&["a.py::one".into(), "a.py::two".into(), "b.py::three".into()]);
        assert_eq!(c.matches("'a.py'").count(), 1, "{c}");
        assert!(c.contains("'b.py'"), "{c}");
    }
}
