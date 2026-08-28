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
pub const PYTEST_FLAGS: &str = "-rA --tb=no -q -p no:cacheprovider";

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

/// The pytest command for a set of node ids.
///
/// Node ids are single-quoted: parametrised tests carry `[]`, which an unquoted shell
/// would glob-expand.
pub fn pytest_command(tests: &[String]) -> String {
    let ids = tests
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("python -m pytest {PYTEST_FLAGS} {ids}")
}

/// Solves an instance by editing the source tree at `workspace`.
pub trait SweSolver {
    fn name(&self) -> &str;
    /// Attempt the fix. `workspace` holds only the source subtree — the tests are not
    /// there and cannot be edited.
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
    let container = InstanceContainer::start(&instance.instance_id, &instance.image())?;

    // 1. Apply the test patch. This is what turns the base commit red: the image ships
    //    the *old* tests, and the patch rewrites them to expect the fixed behaviour.
    let ws = TempWorkspace::new(&instance.instance_id)
        .map_err(|e| DcError::Eval(format!("workspace: {e}")))?;
    let patch_path = ws.path().join("test.patch");
    std::fs::write(&patch_path, &instance.test_patch)
        .map_err(|e| DcError::Eval(format!("writing test patch: {e}")))?;
    container.copy_in(&patch_path, "/tmp/test.patch")?;
    //    Committed, not just applied: the freeze check below asks git what changed,
    //    and an uncommitted test patch would show up as the harness tampering with its
    //    own tests. Committing makes the patched tree the baseline the agent is
    //    measured against.
    let (ok, out) = container.exec(&in_testbed(
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
    let (_, out) = container.exec(&in_testbed(&cmd, TIMEOUT_SECS))?;
    let red = SweScore::grade(instance, &sc_verify::parse(&cmd, &out, false));
    if !red.is_red_start() {
        return Ok(InstanceRun::failed(
            instance,
            format!("not red at setup ({}) — instance unusable", red.line()),
            started,
        ));
    }

    // 3. Hand the agent the source subtree only. The tests stay in the container.
    let src = ws.path().join("src");
    std::fs::create_dir_all(&src).map_err(|e| DcError::Eval(format!("src dir: {e}")))?;
    container.copy_out(&format!("{TESTBED}/{}", instance.src_dir), &src)?;

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
    container.copy_in(&src.join(leaf), &parent)?;

    // 5. Freeze check. The tests were never on the host, so this can only trip if the
    //    agent reached them another way — but an invariant that is merely structural
    //    and never asserted is one nobody notices losing.
    let paths = instance.test_files.join(" ");
    let (_, diff) = container.exec(&in_testbed(
        &format!("git diff --name-only -- {paths}"),
        120,
    ))?;
    let tampered = diff
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string);

    // 6. Green check.
    let (_, out) = container.exec(&in_testbed(&cmd, TIMEOUT_SECS))?;
    let score = SweScore::grade(instance, &sc_verify::parse(&cmd, &out, false));

    Ok(InstanceRun {
        instance_id: instance.instance_id.clone(),
        repo: instance.repo.clone(),
        resolved: score.resolved() && tampered.is_none(),
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
        assert!(c.contains("-rA"), "-rA names the passes: {c}");
        assert!(c.contains("'t.py::a'"), "node ids are quoted: {c}");
        assert!(c.contains("'t.py::b'"));
    }

    /// Node ids carry `[]` (parametrised tests); unquoted they would be glob-expanded
    /// by the shell before pytest ever saw them.
    #[test]
    fn parametrised_node_ids_survive_the_shell() {
        let c = pytest_command(&["t.py::test[compound-model6]".into()]);
        assert!(c.contains("'t.py::test[compound-model6]'"), "{c}");
    }
}
