//! One SWE-bench instance, as vendored into `evals/swebench/lite-subset.json`.

use serde::{Deserialize, Serialize};

/// A single SWE-bench task: a real bug in a real repository, with the tests that
/// prove it fixed and the tests that prove nothing else broke.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SweInstance {
    /// e.g. `pylint-dev__pylint-6506`.
    pub instance_id: String,
    /// e.g. `pylint-dev/pylint`.
    pub repo: String,
    pub base_commit: String,
    /// What the agent is asked to fix — the upstream issue text.
    pub problem_statement: String,
    /// The diff that introduces (or rewrites) the tests. Applied by the harness at
    /// setup; never given to the agent.
    pub test_patch: String,
    /// Tests that must go from failing to passing. The fix.
    pub fail_to_pass: Vec<String>,
    /// Tests that were passing and must still pass. The no-regression guarantee.
    pub pass_to_pass: Vec<String>,
    /// The source subtree handed to the agent, relative to the repo root
    /// (e.g. `pylint`, `src/flask`). See [`SweInstance::image`] for why only this.
    pub src_dir: String,
    /// The files the test patch touches, derived from the node ids.
    pub test_files: Vec<String>,
}

/// The vendored subset, with the provenance needed to report it honestly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Subset {
    pub source: String,
    pub split: String,
    /// How many instances the full split holds — the denominator this subset is
    /// **not**, kept next to the data so a report cannot quietly imply otherwise.
    pub total_in_split: usize,
    pub note: String,
    pub instances: Vec<SweInstance>,
}

impl Subset {
    pub fn parse(text: &str) -> sc_proto::Result<Subset> {
        serde_json::from_str(text)
            .map_err(|e| sc_proto::DcError::Eval(format!("swebench subset: {e}")))
    }

    /// The subset shipped with the crate, compiled in.
    ///
    /// `include_str!` rather than a path read, so a run needs no network and no
    /// working directory: the data a score was produced from is pinned to the commit
    /// that produced it.
    pub fn bundled() -> sc_proto::Result<Subset> {
        Subset::parse(include_str!("../../../../evals/swebench/lite-subset.json"))
    }

    pub fn get(&self, instance_id: &str) -> Option<&SweInstance> {
        self.instances.iter().find(|i| i.instance_id == instance_id)
    }
}

impl SweInstance {
    /// The official pre-built evaluation image for this instance.
    ///
    /// These carry the repo at `base_commit` with its pinned dependencies and the
    /// right Python already installed — the per-task environment isolation spec 07
    /// names as a precondition. We do not build environments; the benchmark authors
    /// did, and using theirs is both less work and more faithful.
    ///
    /// The tag mangles `__` to `_1776_` (SWE-bench's own convention — a Docker tag
    /// cannot carry every character an instance id can).
    pub fn image(&self) -> String {
        format!(
            "swebench/sweb.eval.x86_64.{}:latest",
            self.instance_id.replace("__", "_1776_")
        )
    }

    /// Every test whose result is scored, F2P then P2P.
    pub fn all_tests(&self) -> Vec<String> {
        let mut v = self.fail_to_pass.clone();
        v.extend(self.pass_to_pass.iter().cloned());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_tag_mangles_the_double_underscore() {
        let i = SweInstance {
            instance_id: "pylint-dev__pylint-6506".into(),
            repo: "pylint-dev/pylint".into(),
            base_commit: "x".into(),
            problem_statement: String::new(),
            test_patch: String::new(),
            fail_to_pass: vec![],
            pass_to_pass: vec![],
            src_dir: "pylint".into(),
            test_files: vec![],
        };
        assert_eq!(
            i.image(),
            "swebench/sweb.eval.x86_64.pylint-dev_1776_pylint-6506:latest"
        );
    }

    #[test]
    fn the_bundled_subset_parses_and_is_self_describing() {
        let s = Subset::bundled().expect("bundled subset parses");
        assert!(!s.instances.is_empty());
        // The honest denominator travels with the data.
        assert!(s.total_in_split > s.instances.len());
        assert!(s.note.to_lowercase().contains("not comparable"));

        for i in &s.instances {
            assert!(!i.fail_to_pass.is_empty(), "{} has no F2P", i.instance_id);
            assert!(!i.test_patch.is_empty(), "{} has no patch", i.instance_id);
            assert!(!i.src_dir.is_empty(), "{} has no src_dir", i.instance_id);
        }
    }
}
