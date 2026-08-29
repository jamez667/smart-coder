//! SWE-bench grading: did the fix land, and did anything else break?

use std::collections::BTreeSet;

use serde::Serialize;

use super::instance::SweInstance;

/// The result of scoring one instance's test run against its expected outcomes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SweScore {
    pub f2p_passed: Vec<String>,
    pub f2p_failed: Vec<String>,
    pub p2p_passed: Vec<String>,
    /// Was passing, now fails — a regression the fix caused.
    pub p2p_broken: Vec<String>,
    /// Named in the instance but absent from the run's output entirely: a collection
    /// error, a rename, a crash. **Counted against resolution**, never ignored —
    /// scoring "no failures seen" instead of "every expected pass seen" is how a run
    /// that never executed scores as a clean sweep.
    pub missing: Vec<String>,
}

impl SweScore {
    /// SWE-bench's own definition: every FAIL_TO_PASS passes, every PASS_TO_PASS
    /// still passes, and nothing went unaccounted for.
    ///
    /// Deliberately strict, and deliberately not softened to a ratio — a partial fix
    /// is not a fix, and the benchmark's number means this or it means nothing.
    pub fn resolved(&self) -> bool {
        self.f2p_failed.is_empty() && self.p2p_broken.is_empty() && self.missing.is_empty()
    }

    /// Score a parsed test report against what the instance expects.
    ///
    /// `report` must come from a `pytest -rA` run — see [`super::runner::PYTEST_FLAGS`].
    /// Anything quieter names only the failures, and every P2P would land in
    /// `missing`.
    pub fn grade(instance: &SweInstance, report: &sc_verify::TestReport) -> SweScore {
        let passed: BTreeSet<&str> = report
            .cases
            .iter()
            .filter(|c| c.passed)
            .map(|c| c.name.as_str())
            .collect();
        let failed: BTreeSet<&str> = report
            .cases
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.name.as_str())
            .collect();

        let mut s = SweScore::default();
        for t in &instance.fail_to_pass {
            if passed.contains(t.as_str()) {
                s.f2p_passed.push(t.clone());
            } else if failed.contains(t.as_str()) {
                s.f2p_failed.push(t.clone());
            } else {
                s.missing.push(t.clone());
            }
        }
        for t in &instance.pass_to_pass {
            if passed.contains(t.as_str()) {
                s.p2p_passed.push(t.clone());
            } else if failed.contains(t.as_str()) {
                s.p2p_broken.push(t.clone());
            } else {
                s.missing.push(t.clone());
            }
        }
        s
    }

    /// Whether this looks like the RED state the harness requires before solving:
    /// every F2P failing, and no P2P already broken.
    ///
    /// A run that is already green before the agent touches it is a broken instance,
    /// not an easy one, and must never be reported as solved.
    ///
    /// `missing` is deliberately NOT consulted. An id absent at setup is absent because
    /// it does not exist in the repository — a handful of upstream rows carry ids that
    /// were split on whitespace — and it will be absent at the green check too, where
    /// [`Self::resolved`] still counts it against the model. Excluding the whole
    /// instance for it discards every other test as well: `pallets__flask-5063` scored
    /// `F2P 0/2  P2P 52/52  MISSING 2` and was thrown away over those two.
    ///
    /// The invariant that matters is unchanged — an F2P that already passes, or a P2P
    /// that is already broken, still makes the instance unusable.
    pub fn is_red_start(&self) -> bool {
        self.f2p_passed.is_empty() && self.p2p_broken.is_empty() && !self.f2p_failed.is_empty()
    }

    pub fn line(&self) -> String {
        format!(
            "F2P {}/{}  P2P {}/{}{}",
            self.f2p_passed.len(),
            self.f2p_passed.len() + self.f2p_failed.len(),
            self.p2p_passed.len(),
            self.p2p_passed.len() + self.p2p_broken.len(),
            if self.missing.is_empty() {
                String::new()
            } else {
                format!("  MISSING {}", self.missing.len())
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::instance::Benchmark;
    use super::*;
    use sc_verify::{TestCase, TestReport};

    fn instance() -> SweInstance {
        SweInstance {
            benchmark: Benchmark::SweBench,
            instance_id: "x__y-1".into(),
            repo: "x/y".into(),
            base_commit: "c".into(),
            problem_statement: String::new(),
            test_patch: String::new(),
            fail_to_pass: vec!["t.py::fix_a".into(), "t.py::fix_b".into()],
            pass_to_pass: vec!["t.py::keep_a".into(), "t.py::keep_b".into()],
            src_dir: "y".into(),
            test_files: vec!["t.py".into()],
        }
    }

    fn report(cases: &[(&str, bool)]) -> TestReport {
        TestReport {
            cases: cases
                .iter()
                .map(|(n, p)| TestCase {
                    name: (*n).to_string(),
                    passed: *p,
                    message: None,
                })
                .collect(),
            command_ok: cases.iter().all(|(_, p)| *p),
            generic: false,
            raw: None,
        }
    }

    #[test]
    fn everything_green_resolves() {
        let s = SweScore::grade(
            &instance(),
            &report(&[
                ("t.py::fix_a", true),
                ("t.py::fix_b", true),
                ("t.py::keep_a", true),
                ("t.py::keep_b", true),
            ]),
        );
        assert!(s.resolved(), "{s:?}");
    }

    #[test]
    fn one_unfixed_test_is_not_resolved() {
        let s = SweScore::grade(
            &instance(),
            &report(&[
                ("t.py::fix_a", true),
                ("t.py::fix_b", false),
                ("t.py::keep_a", true),
                ("t.py::keep_b", true),
            ]),
        );
        assert!(!s.resolved());
        assert_eq!(s.f2p_failed, ["t.py::fix_b"]);
    }

    /// Fixing the bug by breaking something else is not a fix.
    #[test]
    fn a_regression_in_pass_to_pass_is_not_resolved() {
        let s = SweScore::grade(
            &instance(),
            &report(&[
                ("t.py::fix_a", true),
                ("t.py::fix_b", true),
                ("t.py::keep_a", true),
                ("t.py::keep_b", false),
            ]),
        );
        assert!(!s.resolved());
        assert_eq!(s.p2p_broken, ["t.py::keep_b"]);
    }

    /// The failure mode this type exists to prevent: a run that produced no output
    /// (collection error, crash, wrong node ids) has no failures to see, and must not
    /// therefore look like a clean sweep.
    #[test]
    fn a_run_that_named_nothing_is_missing_not_resolved() {
        let s = SweScore::grade(&instance(), &report(&[]));
        assert!(!s.resolved());
        assert_eq!(s.missing.len(), 4);
        assert!(s.line().contains("MISSING 4"));
    }

    /// A parser that only names failures (quiet-mode pytest) puts every expected pass
    /// in `missing` — which is exactly why the runner forces `-rA`.
    #[test]
    fn failures_only_output_does_not_resolve() {
        let s = SweScore::grade(&instance(), &report(&[("t.py::fix_a", false)]));
        assert!(!s.resolved());
        assert_eq!(s.missing.len(), 3);
    }

    /// A `missing` id at setup must not throw the whole instance away.
    ///
    /// `pallets__flask-5063` carries two ids that were split on whitespace upstream.
    /// They can never be found, and `resolved()` still counts them against the model —
    /// but excluding the instance over them discarded its other 54 tests too.
    #[test]
    fn a_permanently_missing_id_does_not_make_the_instance_unusable() {
        let i = SweInstance {
            fail_to_pass: vec!["t.py::fix_a".into(), "t.py::fix_b".into()],
            pass_to_pass: vec!["t.py::keep_a".into(), "t.py::gone[split".into()],
            ..instance()
        };
        // Setup: both fixes fail, the real keep passes, the malformed id is absent.
        let red = SweScore::grade(
            &i,
            &report(&[
                ("t.py::fix_a", false),
                ("t.py::fix_b", false),
                ("t.py::keep_a", true),
            ]),
        );
        assert_eq!(red.missing, ["t.py::gone[split"]);
        assert!(red.is_red_start(), "usable despite the missing id: {red:?}");
        // But it still counts against resolution.
        assert!(!red.resolved());
    }

    /// If NOTHING was verified, the instance is unusable — an empty red check is not a
    /// red check.
    #[test]
    fn an_instance_whose_fixes_all_went_missing_is_unusable() {
        let red = SweScore::grade(&instance(), &report(&[]));
        assert!(red.f2p_failed.is_empty());
        assert!(
            !red.is_red_start(),
            "nothing ran, so nothing was proven red"
        );
    }

    #[test]
    fn the_red_start_is_every_fix_failing_and_every_keep_passing() {
        let red = SweScore::grade(
            &instance(),
            &report(&[
                ("t.py::fix_a", false),
                ("t.py::fix_b", false),
                ("t.py::keep_a", true),
                ("t.py::keep_b", true),
            ]),
        );
        assert!(red.is_red_start());
        assert!(!red.resolved());

        // Already-green F2P at setup: a broken instance, not an easy one.
        let green = SweScore::grade(
            &instance(),
            &report(&[
                ("t.py::fix_a", true),
                ("t.py::fix_b", false),
                ("t.py::keep_a", true),
                ("t.py::keep_b", true),
            ]),
        );
        assert!(!green.is_red_start());
    }
}
