//! One SWE-bench instance, as vendored into `evals/swebench/lite-subset.json`.

use serde::{Deserialize, Serialize};

/// Which benchmark an instance came from.
///
/// They differ in two ways that matter to the runner, and in nothing else: who
/// publishes the images, and how Python is reached inside them. Everything else — the
/// red-first invariant, the F2P/P2P grading, the frozen tests — is identical, so this
/// is a two-field difference rather than a second harness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Benchmark {
    /// `princeton-nlp/SWE-bench_Lite`. Images under `swebench/`, conda env `testbed`.
    #[default]
    SweBench,
    /// `SWE-bench-Live/SWE-bench-Live`. Images under `starryzhang/`, plain system
    /// Python at `/usr/local/bin` with no environment to activate.
    SweBenchLive,
}

impl Benchmark {
    pub fn image_owner(self) -> &'static str {
        match self {
            Benchmark::SweBench => "swebench",
            Benchmark::SweBenchLive => "starryzhang",
        }
    }

    /// Shell prefix that puts the project's `python` on PATH inside the image.
    ///
    /// SWE-bench images are uniform: conda, env `testbed`. Their `sh` is dash, which
    /// has no `source`, hence `.`.
    ///
    /// SWE-bench-Live images are **not** uniform — they are built per project, so the
    /// environment is whatever that project uses. Most put the deps in the system
    /// Python and need no prefix; `run-llama/llama_deploy` manages its own with poetry,
    /// where the system Python has poetry's dependencies and not the project's, so a
    /// bare `python -m pytest` fails with "No module named pytest" and every test
    /// reports `missing`. Hence the shell probe: use `poetry run` when there is a
    /// `poetry.lock` and poetry is installed, otherwise nothing.
    ///
    /// Probing beats a per-repo table because the next Live repo added will be some
    /// other tool again (uv, pipenv, tox); this at least fails loudly rather than
    /// silently scoring zero.
    pub fn python_prefix(self) -> &'static str {
        match self {
            Benchmark::SweBench => {
                ". /opt/miniconda3/etc/profile.d/conda.sh && conda activate testbed && "
            }
            Benchmark::SweBenchLive => {
                "if [ -f poetry.lock ] && command -v poetry >/dev/null 2>&1; then \
                 SC_PY='poetry run'; else SC_PY=''; fi; "
            }
        }
    }
}

/// A single SWE-bench task: a real bug in a real repository, with the tests that
/// prove it fixed and the tests that prove nothing else broke.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SweInstance {
    /// Which benchmark this row came from. Defaults to `SweBench` so existing
    /// vendored files load unchanged.
    #[serde(default)]
    pub benchmark: Benchmark,
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

    /// The SWE-bench-**Live** subset, also compiled in.
    ///
    /// A different benchmark from Lite — newer issues, drawn from recent GitHub
    /// activity to limit training contamination. Kept in its own file (and reported
    /// under its own `source`) precisely so the two can never be averaged together.
    pub fn bundled_live() -> sc_proto::Result<Subset> {
        Subset::parse(include_str!("../../../../evals/swebench/live-subset.json"))
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
    /// cannot carry every character an instance id can). The Docker Hub *owner*
    /// differs per benchmark, so it travels with the instance rather than being
    /// hardcoded: SWE-bench publishes under `swebench/`, SWE-bench-Live under
    /// `starryzhang/`.
    pub fn image(&self) -> String {
        format!(
            "{}/sweb.eval.x86_64.{}:latest",
            self.benchmark.image_owner(),
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
            benchmark: Benchmark::SweBench,
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
