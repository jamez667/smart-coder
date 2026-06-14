//! Structured test results (spec 04 — `run_verification` returns per-test
//! pass/fail, not a 5k-line log; spec 11 — the test is the oracle).
//!
//! A small window must be spent on *what's broken*, not a wall of green
//! ([05](../../docs/specs/05-context-management.md)): the report carries the
//! failing cases with their messages, and renders an observation that leads with
//! failures and truncates the passing noise.

/// The outcome of a single test case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub name: String,
    pub passed: bool,
    /// Failure message / assertion detail, when failed.
    pub message: Option<String>,
}

/// The structured result of running a verification command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReport {
    /// Per-test outcomes, when the framework's output could be parsed.
    pub cases: Vec<TestCase>,
    /// Whether the command exited 0 (the whole-suite gate, spec 11).
    pub command_ok: bool,
    /// True when no per-test breakdown could be parsed and we fell back to the
    /// raw exit code (the generic path).
    pub generic: bool,
}

impl TestReport {
    /// A generic pass/fail with no per-test detail (exit-code fallback).
    pub fn generic(command_ok: bool) -> Self {
        Self {
            cases: Vec::new(),
            command_ok,
            generic: true,
        }
    }

    pub fn passed_count(&self) -> usize {
        self.cases.iter().filter(|c| c.passed).count()
    }

    pub fn failed(&self) -> Vec<&TestCase> {
        self.cases.iter().filter(|c| !c.passed).collect()
    }

    /// The whole-suite gate (spec 11): every parsed test green *and* the command
    /// exited 0. For a generic report, just the exit code.
    pub fn all_green(&self) -> bool {
        self.command_ok && self.failed().is_empty()
    }

    /// A compact, failure-first observation for the model. Leads with failing
    /// cases and their messages; summarizes the passing ones rather than listing
    /// them (spec 05 — spend the window on what's broken).
    pub fn observation(&self) -> String {
        if self.generic {
            return if self.command_ok {
                "run_verification: command exited 0 (passed); no per-test detail available".into()
            } else {
                "run_verification: command exited non-zero (failed); no per-test detail available"
                    .into()
            };
        }
        let failed = self.failed();
        let passed = self.passed_count();
        if failed.is_empty() {
            return format!("run_verification: all {passed} test(s) passed ✓");
        }
        let mut out = format!(
            "run_verification: {} failed, {passed} passed:\n",
            failed.len()
        );
        for c in &failed {
            out.push_str(&format!("✗ {}", c.name));
            if let Some(m) = &c.message {
                out.push_str(&format!("\n    {}", m.replace('\n', "\n    ")));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, passed: bool) -> TestCase {
        TestCase {
            name: name.into(),
            passed,
            message: if passed {
                None
            } else {
                Some("assertion failed".into())
            },
        }
    }

    #[test]
    fn all_green_requires_command_ok_and_no_failures() {
        let green = TestReport {
            cases: vec![case("a", true), case("b", true)],
            command_ok: true,
            generic: false,
        };
        assert!(green.all_green());

        let red = TestReport {
            cases: vec![case("a", true), case("b", false)],
            command_ok: false,
            generic: false,
        };
        assert!(!red.all_green());
        assert_eq!(red.failed().len(), 1);
    }

    #[test]
    fn observation_leads_with_failures() {
        let red = TestReport {
            cases: vec![case("keep", true), case("broken", false)],
            command_ok: false,
            generic: false,
        };
        let o = red.observation();
        assert!(o.contains("✗ broken"), "{o}");
        assert!(o.contains("1 failed, 1 passed"), "{o}");
        // The passing test isn't individually listed.
        assert!(!o.contains("✗ keep"), "{o}");
    }

    #[test]
    fn generic_report_uses_exit_code_only() {
        assert!(TestReport::generic(true).all_green());
        assert!(!TestReport::generic(false).all_green());
        assert!(TestReport::generic(true).observation().contains("exited 0"));
    }
}
