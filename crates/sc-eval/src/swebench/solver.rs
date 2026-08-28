//! Solvers for SWE-bench instances.
//!
//! The two here are not models: they are the harness's own known answers. A harness
//! that cannot score a correct fix as resolved and an absent fix as unresolved is not
//! measuring anything, and finding that out from a model run would be far too late.

use std::cell::RefCell;
use std::path::Path;

use sc_proto::Result;

use super::instance::SweInstance;
use super::runner::{SolveReport, SweSolver};

/// Changes nothing. Must score unresolved.
#[derive(Debug, Default)]
pub struct NoopSweSolver;

impl SweSolver for NoopSweSolver {
    fn name(&self) -> &str {
        "noop"
    }
    fn solve(&self, _instance: &SweInstance, _workspace: &Path) -> Result<()> {
        Ok(())
    }
}

/// Applies a patch supplied by the caller. Must score resolved when given the gold
/// patch, which makes the whole pipeline a known-answer test.
///
/// The gold patch is deliberately **not** vendored with the instances: it is the
/// answer, and a file holding it next to the tasks is one accidental context-injection
/// away from being handed to a model. The live test fetches it at run time.
pub struct GoldPatchSolver {
    diff: String,
    applied: RefCell<bool>,
}

impl GoldPatchSolver {
    pub fn new(diff: impl Into<String>) -> Self {
        Self {
            diff: diff.into(),
            applied: RefCell::new(false),
        }
    }

    pub fn applied(&self) -> bool {
        *self.applied.borrow()
    }
}

impl SweSolver for GoldPatchSolver {
    fn name(&self) -> &str {
        "gold-patch"
    }

    /// Patch paths are repo-relative under `a/` (`a/pylint/config/x.py`), and the
    /// workspace holds the subtree's leaf directory at its root (`pylint/`). For a
    /// top-level `src_dir` those line up at `-p1`.
    ///
    /// A nested one (`src/flask`) does not: the patch says `a/src/flask/x.py` but the
    /// workspace root holds `flask/`, so the extra components are stripped and the
    /// leaf put back with `--directory`.
    fn solve(&self, instance: &SweInstance, workspace: &Path) -> Result<()> {
        let patch = workspace.join("gold.patch");
        std::fs::write(&patch, &self.diff)
            .map_err(|e| sc_proto::DcError::Eval(format!("writing gold patch: {e}")))?;

        let nested = instance.src_dir.matches('/').count();
        let mut cmd = std::process::Command::new("git");
        cmd.arg("apply").arg(format!("-p{}", 1 + nested));
        if nested > 0 {
            let leaf = instance
                .src_dir
                .rsplit('/')
                .next()
                .unwrap_or(&instance.src_dir);
            cmd.arg("--directory").arg(leaf);
        }
        let out = cmd
            .arg(&patch)
            .current_dir(workspace)
            .output()
            .map_err(|e| sc_proto::DcError::Eval(format!("git apply: {e}")))?;
        let _ = std::fs::remove_file(&patch);

        if !out.status.success() {
            return Err(sc_proto::DcError::Eval(format!(
                "gold patch did not apply: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        *self.applied.borrow_mut() = true;
        Ok(())
    }

    fn last_report(&self) -> Option<SolveReport> {
        Some(SolveReport {
            steps: 1,
            stop_reason: "gold-patch".into(),
            ..Default::default()
        })
    }
}
