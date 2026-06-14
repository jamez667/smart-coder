//! Aggregate per-task results into a human-readable report. The headline metric
//! is pass rate (pass@1 with a deterministic solver), per spec 07 / spec 11.

use std::fmt::Write as _;

use dc_core::ToolCallMetrics;

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

    /// Tool-call metrics summed across every model-driven task in the suite. This
    /// is the source for the M1 ≥95% valid-call rate (spec 07). Tasks with no
    /// metrics (non-agent solvers, or runs that never reached solving) contribute
    /// nothing.
    pub fn tool_call_metrics(&self) -> ToolCallMetrics {
        let mut agg = ToolCallMetrics::default();
        for r in &self.results {
            if let Some(m) = &r.metrics {
                agg.merge(m);
            }
        }
        agg
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
        // Surface the M1 tool-call validity rate when there's anything to report.
        let m = self.tool_call_metrics();
        if m.total() > 0 {
            let _ = write!(
                s,
                "\ntool calls: {}/{} valid ({:.1}%)",
                m.valid,
                m.total(),
                m.valid_rate() * 100.0
            );
        }
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
            metrics: None,
        }
    }

    fn res_with(id: &str, outcome: Outcome, metrics: ToolCallMetrics) -> TaskResult {
        TaskResult {
            id: id.into(),
            solver: "agent".into(),
            outcome,
            metrics: Some(metrics),
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
    fn aggregates_tool_call_metrics_across_the_suite() {
        let report = Report::new(vec![
            res_with(
                "a",
                Outcome::Pass,
                ToolCallMetrics {
                    valid: 9,
                    invalid: 1,
                },
            ),
            res_with(
                "b",
                Outcome::Pass,
                ToolCallMetrics {
                    valid: 10,
                    invalid: 0,
                },
            ),
            res("c", Outcome::StillRed), // no metrics — contributes nothing
        ]);
        let m = report.tool_call_metrics();
        assert_eq!(m.total(), 20);
        assert_eq!(m.valid, 19);
        assert!((m.valid_rate() - 0.95).abs() < f64::EPSILON);
        assert!(report.summary().contains("19/20 valid"));
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
