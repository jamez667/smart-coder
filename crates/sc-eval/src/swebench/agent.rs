//! The real agent loop as a SWE-bench solver.

use std::cell::RefCell;
use std::path::Path;

use sc_core::{run_agent, AgentConfig};
use sc_model::ModelBackend;
use sc_proto::Result;
use sc_tools::PermissionPolicy;

use super::instance::SweInstance;
use super::runner::{pytest_command, SolveReport, SweSolver};

/// Drives `sc_core::run_agent` against a real backend.
///
/// The config differs from [`AgentConfig::default`] in four ways, each because the
/// defaults are tuned for the small self-contained tasks of the existing suite rather
/// than a real repository:
///
/// - **`max_steps`** — 25 leaves almost nothing after a couple of failed edits, and a
///   real fix is locate, read, edit, verify, re-edit.
/// - **`observation_line_cap`** — 40 lines truncates a pytest failure mid-traceback,
///   amputating the assertion that names the bug.
/// - **`read_file_line_cap`** — real modules run past 400 lines.
/// - **`verify_command`** — this is what lets the agent run the tests itself, through
///   `run_verification`, *without* shell access. Shell stays denied: granting it inside
///   a scored eval invites the model to `git checkout` its way to a false pass.
pub struct SweAgentSolver<'a> {
    backend: &'a dyn ModelBackend,
    max_steps: usize,
    verbose: bool,
    last: RefCell<Option<SolveReport>>,
}

impl<'a> SweAgentSolver<'a> {
    pub fn new(backend: &'a dyn ModelBackend) -> Self {
        Self {
            backend,
            max_steps: 60,
            verbose: false,
            last: RefCell::new(None),
        }
    }

    /// Print what the model saw and did each turn (spec 06 --verbose).
    pub fn with_verbose(mut self, on: bool) -> Self {
        self.verbose = on;
        self
    }

    pub fn with_max_steps(mut self, steps: usize) -> Self {
        self.max_steps = steps;
        self
    }

    pub fn config_for(&self, instance: &SweInstance, workspace: &Path) -> AgentConfig {
        AgentConfig {
            max_steps: self.max_steps,
            verbose: self.verbose,
            observation_line_cap: 200,
            read_file_line_cap: 800,
            // The agent runs the tests through `run_verification`, never the shell —
            // and it runs them for real, in the instance container, against the same
            // node ids the harness will score. `sandbox` below is what makes that
            // true; without it this would execute on a Windows host with no Python
            // and the model would be flying blind.
            verify_command: Some(verify_script(
                &instance.src_dir,
                &pytest_command(&instance.fail_to_pass),
            )),
            // `Session` execs into the already-running instance container. It is
            // addressed by a hash of the workspace path, which the runner arranged for
            // by naming the container to match.
            sandbox: sc_verify::Sandbox::Session(sc_verify::SessionContainer::new(
                workspace,
                instance.image(),
            )),
            // The tests ARE on the host now (the model must read them), so this is
            // the live guard against editing them, not merely defence in depth. The
            // `git diff` check after the solve is the backstop, since matching here is
            // exact-string rather than glob.
            permission: PermissionPolicy::with_frozen(instance.test_files.clone()),
            ..AgentConfig::default()
        }
    }

    /// What the model is asked to do.
    ///
    /// The upstream issue text, the tests that must pass, and where to read them.
    /// Naming the test FILES matters: reading the failing test is the model's first
    /// and best move, and it should not have to discover the path.
    pub fn instruction_for(&self, instance: &SweInstance) -> String {
        format!(
            "Fix the following issue in this codebase.\n\n\
             {}\n\n\
             These tests must pass:\n{}\n\n\
             You can READ the tests ({}) to see exactly what behaviour is expected, \
             but you must NOT edit them — fix the source so the tests pass as \
             written.\n",
            instance.problem_statement.trim(),
            instance
                .fail_to_pass
                .iter()
                .map(|t| format!("  - {t}"))
                .collect::<Vec<_>>()
                .join("\n"),
            instance.test_files.join(", "),
        )
    }
}

/// Wrap the agent's test command so it measures what the agent actually did.
///
/// Two things have to happen before pytest runs, and both are easy to lose:
///
/// 1. **Sync the edits in.** The agent edits the source subtree on the host; the tests
///    run against `/testbed` inside the container. Without the copy, verification
///    reports on the container's untouched copy and returns the *same answer every
///    turn* however much the model changes — which the loop correctly reads as making
///    no progress, and stops. The measurement then says nothing about the model.
/// 2. **Activate conda.** `Sandbox::Session` execs with `sh -c`, and `sh` here is dash,
///    which has no `source`. Without it `python` is the system one, missing every
///    pinned dependency.
fn verify_script(src_dir: &str, cmd: &str) -> String {
    let leaf = src_dir.rsplit('/').next().unwrap_or(src_dir);
    let dest = match src_dir.rsplit_once('/') {
        Some((parent, _)) => format!("/testbed/{parent}"),
        None => "/testbed".to_string(),
    };
    format!(
        "cp -r {mount}/src/{leaf} {dest}/ &&          . /opt/miniconda3/etc/profile.d/conda.sh && conda activate testbed && {cmd}",
        mount = super::container::InstanceContainer::HOST_MOUNT,
    )
}

impl SweSolver for SweAgentSolver<'_> {
    fn name(&self) -> &str {
        "agent"
    }

    fn solve(&self, instance: &SweInstance, workspace: &Path) -> Result<()> {
        let cfg = self.config_for(instance, workspace);
        let report = run_agent(
            self.backend,
            &self.instruction_for(instance),
            workspace,
            &cfg,
        )?;
        // Keep the whole report, not just the metrics: `stop_reason` and `steps` are
        // what make an unresolved instance diagnosable rather than merely a zero.
        *self.last.borrow_mut() = Some(SolveReport {
            steps: report.steps,
            stop_reason: format!("{:?}", report.stop_reason),
            tool_calls_valid: report.metrics.valid,
            tool_calls_invalid: report.metrics.invalid,
        });
        Ok(())
    }

    fn last_report(&self) -> Option<SolveReport> {
        self.last.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> SweInstance {
        SweInstance {
            instance_id: "x__y-1".into(),
            repo: "x/y".into(),
            base_commit: "c".into(),
            problem_statement: "Widgets explode when frobnicated.".into(),
            test_patch: String::new(),
            fail_to_pass: vec!["t.py::test_frob".into()],
            pass_to_pass: vec![],
            src_dir: "y".into(),
            test_files: vec!["t.py".into()],
        }
    }

    /// These tests only inspect the config and the instruction, so the backend is
    /// never called — `CallbackBackend` is the existing seam for exactly this.
    fn dummy() -> impl ModelBackend {
        sc_model::CallbackBackend::new(
            "dummy",
            sc_model::Capabilities {
                max_context_tokens: 8192,
                tool_calling: sc_model::ToolCalling::None,
                on_device: true,
            },
            |_: &sc_model::GenerateRequest| unreachable!("config-only test"),
        )
    }

    #[test]
    fn the_instruction_carries_the_issue_and_names_the_tests() {
        let b = dummy();
        let s = SweAgentSolver::new(&b);
        let i = s.instruction_for(&instance());
        assert!(i.contains("Widgets explode"));
        assert!(i.contains("t.py::test_frob"));
        assert!(i.contains("NOT edit them"), "{i}");
        // The test file is named so the model knows where to look — withholding it
        // was what made the task near-unsolvable.
        assert!(i.contains("t.py"), "names the readable test file: {i}");
    }

    /// Shell stays denied inside a scored eval — the agent verifies through
    /// `run_verification`, which is not shell-gated.
    #[test]
    fn the_agent_gets_no_shell_and_the_tests_are_frozen() {
        let b = dummy();
        let s = SweAgentSolver::new(&b);
        let c = s.config_for(&instance(), Path::new("/tmp/ws"));
        assert!(!c.permission.allow_shell);
        assert!(c.permission.shell_allowlist.is_empty());
        assert_eq!(c.permission.frozen_paths, ["t.py"]);
        assert!(c.verify_command.is_some());
    }

    /// The regression this guards: without the copy, the agent's own verification
    /// reports on the container's untouched source and returns the same answer every
    /// turn. Measured before the fix — 38 steps, then `Stalled("many turns with no
    /// change to the workspace")`, on an instance the gold patch resolves.
    #[test]
    fn verification_syncs_the_agents_edits_before_running_tests() {
        let script = verify_script("pylint", "python -m pytest -rA 'x::y'");
        assert!(
            script.starts_with("cp -r /hostws/src/pylint /testbed/"),
            "the edits are copied in first: {script}"
        );
        assert!(script.contains("conda activate testbed"));
        assert!(
            !script.contains("source "),
            "dash has no `source`: {script}"
        );
        // Order matters: a sync after the tests would measure the previous turn.
        assert!(script.find("cp -r").unwrap() < script.find("pytest").unwrap());
    }

    /// A nested subtree (`src/flask`) lands beside its siblings, not on top of them.
    #[test]
    fn a_nested_source_dir_is_copied_to_its_parent() {
        let script = verify_script("src/flask", "pytest");
        assert!(
            script.starts_with("cp -r /hostws/src/flask /testbed/src/"),
            "{script}"
        );
    }

    /// The defaults are tuned for toy tasks; a real repo needs more room.
    #[test]
    fn the_budgets_are_raised_above_the_toy_task_defaults() {
        let d = AgentConfig::default();
        let b = dummy();
        let c = SweAgentSolver::new(&b).config_for(&instance(), Path::new("/tmp/ws"));
        assert!(c.max_steps > d.max_steps);
        assert!(c.observation_line_cap > d.observation_line_cap);
        assert!(c.read_file_line_cap > d.read_file_line_cap);
    }
}
