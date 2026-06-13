//! Aggregate per-task results into a human-readable report. The headline metric
//! is pass rate (pass@1 with a deterministic solver), per spec 07 / spec 11.

use std::fmt::Write as _;

use crate::runner::{Outcome, TaskResult};

/// A scored suite run.
pub struct Report {
    pub results: Vec<TaskResult>,
}

impl Report {
    pub fn new(results: Vec<TaskResult>) -> Self {
        Self { results }
    }

    pub fn total(&self) -> usize {
        self.results.len()
    }

    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.outcome.is_pass()).count()
    }

    /// Fraction in [0.0, 1.0]; 0.0 for an empty suite.
    pub fn pass_rate(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.passed() as f64 / self.total() as f64
    }

    /// True only if every task passed (the suite's overall gate).
    pub fn all_passed(&self) -> bool {
        !self.results.is_empty() && self.passed() == self.total()
    }

    /// A multi-line human summary.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        for r in &self.results {
            let detail = match &r.outcome {
                Outcome::ContractTampered(p) => format!(" ({p})"),
                Outcome::SolverError(m) | Outcome::HarnessError(m) => format!(" ({m})"),
                _ => String::new(),
            };
            let _ = writeln!(s, "  [{}] {}{}", r.outcome.symbol(), r.id, detail);
        }
        let _ = write!(
            s,
            "{}/{} passed ({:.0}%)",
            self.passed(),
            self.total(),
            self.pass_rate() * 100.0
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(id: &str, outcome: Outcome) -> TaskResult {
        TaskResult {
            id: id.into(),
            solver: "t".into(),
            outcome,
        }
    }

    #[test]
    fn computes_pass_rate_and_gate() {
        let report = Report::new(vec![res("a", Outcome::Pass), res("b", Outcome::StillRed)]);
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 1);
        assert!((report.pass_rate() - 0.5).abs() < f64::EPSILON);
        assert!(!report.all_passed());
    }

    #[test]
    fn all_passed_requires_nonempty_and_full() {
        assert!(!Report::new(vec![]).all_passed());
        assert!(Report::new(vec![res("a", Outcome::Pass)]).all_passed());
    }

    #[test]
    fn summary_mentions_each_task() {
        let report = Report::new(vec![
            res("alpha", Outcome::Pass),
            res("beta", Outcome::ContractTampered("test.sh".into())),
        ]);
        let s = report.summary();
        assert!(s.contains("alpha"));
        assert!(s.contains("beta"));
        assert!(s.contains("test.sh"));
        assert!(s.contains("1/2 passed"));
    }
}
