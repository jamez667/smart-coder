//! The eval task format. A task is a small repo fixture in a known *red* state
//! (a failing test) plus how to verify it and which tests are frozen.
//!
//! See spec 07 (fixed task suite, tracked from M1) and spec 11 (TDD).

use std::path::{Path, PathBuf};

use sc_proto::{DcError, Result};
use serde::Deserialize;

/// One red->green eval task.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalTask {
    /// Stable identifier, used in reports.
    pub id: String,
    /// Human description of the change to make.
    pub description: String,
    /// Directory holding the starting repo state (must be *red*: its test fails).
    /// Resolved to an absolute path by [`TaskSuite::load`].
    pub fixture: PathBuf,
    /// Shell command run inside the workspace; exit code 0 == green.
    pub verify_cmd: String,
    /// Workspace-relative paths a solver must not modify (frozen contract tests).
    #[serde(default)]
    pub contract_tests: Vec<String>,
    /// Optional known-good solution directory, copied over the workspace by the
    /// demo [`crate::solver::FileSolver`]. Real solvers ignore this. Resolved to
    /// an absolute path by [`TaskSuite::load`].
    #[serde(default)]
    pub solution: Option<PathBuf>,
    /// Free-form labels for grouping results, e.g. `["rung:symptomatic", "lang:rust"]`.
    ///
    /// The ladder needs these: a single pass/fail total says a model scored 4/6 but
    /// not *which* rung it fell off, and the rung is the whole measurement. Additive
    /// and untyped on purpose -- a new axis should not need a schema change.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Per-task override for the verify timeout, in seconds.
    ///
    /// The default is generous enough for a cold cargo build; a task that should be
    /// fast can say so, and catch a runaway sooner.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A suite of tasks, loaded from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskSuite {
    pub tasks: Vec<EvalTask>,
}

impl TaskSuite {
    /// Parse a suite from a TOML string. Paths are left as-written (relative).
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| DcError::Eval(format!("parsing task suite: {e}")))
    }

    /// Load a suite from `path`, resolving `fixture`/`solution` paths relative to
    /// the suite file's directory so a suite is portable.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DcError::Eval(format!("reading {}: {e}", path.display())))?;
        let mut suite = Self::from_toml_str(&text)?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        for t in &mut suite.tasks {
            t.fixture = base.join(&t.fixture);
            t.solution = t.solution.as_ref().map(|s| base.join(s));
        }
        Ok(suite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_suite() {
        let s = r#"
            [[tasks]]
            id = "demo"
            description = "make it green"
            fixture = "tasks/demo/fixture"
            verify_cmd = "sh test.sh"
            contract_tests = ["test.sh"]
        "#;
        let suite = TaskSuite::from_toml_str(s).unwrap();
        assert_eq!(suite.tasks.len(), 1);
        let t = &suite.tasks[0];
        assert_eq!(t.id, "demo");
        assert_eq!(t.contract_tests, vec!["test.sh".to_string()]);
        assert!(t.solution.is_none());
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(TaskSuite::from_toml_str("this is not = = toml").is_err());
    }
}
