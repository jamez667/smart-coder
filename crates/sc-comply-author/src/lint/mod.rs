//! Deterministic lints over a compliance pack.
//!
//! A lint that *proves* a check is broken beats a model that *thinks* it looks
//! odd, every time. Most of what makes a pack wrong is mechanically decidable —
//! a regex that can never match, a glob that selects nothing, an `on_no_files`
//! left unset on a path that is usually absent — so the bulk of this crate is
//! plain analysis with no model involved.
//!
//! Every lint is a pure function over a parsed [`Pack`] plus an optional sample
//! workspace, which makes the whole surface unit-testable without a network.
//!
//! See `docs/specs/14-pack-authoring.md`.

pub mod outcomes;
pub mod patterns;
pub mod structure;

use sc_comply::pack::{Check, Control, Pack};
use sc_comply::status::Severity;

use crate::sample::Sample;

/// One thing wrong (or suspicious) about a pack.
///
/// Reuses `sc-comply`'s own [`Severity`] rather than inventing a parallel scale:
/// the tool that critiques evidence packs should speak the same vocabulary as
/// the packs it critiques.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    /// Stable kebab-case lint name, e.g. `"unset-on-no-files-on-absent-path"`.
    pub lint: &'static str,
    pub severity: Severity,
    /// `"CC8.1/pr-review-required"`, or just the control id for control-level
    /// findings. The locator an author uses to find the thing.
    pub locus: String,
    /// What is wrong, in one sentence.
    pub summary: String,
    /// Why it matters — the consequence for an auditor reading the report.
    pub consequence: String,
    /// What to do about it. Concrete, not "consider reviewing".
    pub suggestion: String,
}

impl LintFinding {
    pub fn new(
        lint: &'static str,
        severity: Severity,
        locus: impl Into<String>,
        summary: impl Into<String>,
        consequence: impl Into<String>,
        suggestion: impl Into<String>,
    ) -> Self {
        LintFinding {
            lint,
            severity,
            locus: locus.into(),
            summary: summary.into(),
            consequence: consequence.into(),
            suggestion: suggestion.into(),
        }
    }
}

/// Everything the lints found, worst first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LintReport {
    pub findings: Vec<LintFinding>,
    /// The pack that was linted, for the report header.
    pub framework: String,
    /// Whether a sample workspace was supplied.
    ///
    /// Several lints can only run against real files; without a sample they are
    /// skipped, and a report that does not say so would imply a clean bill of
    /// health it has not earned.
    pub had_sample: bool,
}

impl LintReport {
    /// Findings at or above `min`, worst first then by locus for stability.
    pub fn at_least(&self, min: Severity) -> Vec<&LintFinding> {
        let mut v: Vec<&LintFinding> = self.findings.iter().filter(|f| f.severity >= min).collect();
        v.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.locus.cmp(&b.locus)));
        v
    }

    /// Count at a given severity.
    pub fn count(&self, sev: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == sev).count()
    }

    /// Findings serious enough to block using the pack as-is.
    pub fn blocking(&self) -> Vec<&LintFinding> {
        self.at_least(Severity::High)
    }

    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// One-line count summary.
    pub fn summary_line(&self) -> String {
        format!(
            "{} critical · {} high · {} medium · {} low",
            self.count(Severity::Critical),
            self.count(Severity::High),
            self.count(Severity::Medium),
            self.count(Severity::Low),
        )
    }
}

/// Context handed to every lint.
pub struct LintCtx<'a> {
    pub pack: &'a Pack,
    /// A real workspace to test globs and paths against.
    ///
    /// `None` means the file-dependent lints cannot run. They must skip rather
    /// than guess — a lint that reports "this glob matches nothing" when it had
    /// nothing to match against is worse than silence.
    pub sample: Option<&'a Sample>,
}

impl<'a> LintCtx<'a> {
    pub fn new(pack: &'a Pack, sample: Option<&'a Sample>) -> Self {
        LintCtx { pack, sample }
    }

    /// Iterate `(control, check)` pairs across the pack.
    pub fn checks(&self) -> impl Iterator<Item = (&'a Control, &'a Check)> {
        self.pack
            .controls
            .iter()
            .flat_map(|c| c.checks.iter().map(move |k| (c, k)))
    }
}

/// The qualified locator for a check, matching how evidence is cited in a run.
pub fn locus(control: &Control, check: &Check) -> String {
    format!("{}/{}", control.id, check.id)
}

/// Run every lint.
pub fn lint_pack(pack: &Pack, sample: Option<&Sample>) -> LintReport {
    let ctx = LintCtx::new(pack, sample);
    let mut findings = Vec::new();

    outcomes::run(&ctx, &mut findings);
    patterns::run(&ctx, &mut findings);
    structure::run(&ctx, &mut findings);

    // Deterministic ordering so a report diffs cleanly between runs.
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.locus.cmp(&b.locus))
            .then(a.lint.cmp(b.lint))
    });

    LintReport {
        findings,
        framework: format!("{} {}", pack.framework.name, pack.framework.version),
        had_sample: sample.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(lint: &'static str, sev: Severity, locus: &str) -> LintFinding {
        LintFinding::new(lint, sev, locus, "s", "c", "f")
    }

    #[test]
    fn at_least_filters_and_orders_worst_first() {
        let r = LintReport {
            findings: vec![
                finding("a", Severity::Low, "Z"),
                finding("b", Severity::Critical, "Y"),
                finding("c", Severity::Medium, "X"),
            ],
            framework: "F".into(),
            had_sample: true,
        };
        let got: Vec<&str> = r
            .at_least(Severity::Medium)
            .iter()
            .map(|f| f.lint)
            .collect();
        assert_eq!(got, vec!["b", "c"]);
    }

    #[test]
    fn blocking_is_high_and_above() {
        let r = LintReport {
            findings: vec![
                finding("low", Severity::Low, "A"),
                finding("med", Severity::Medium, "B"),
                finding("high", Severity::High, "C"),
                finding("crit", Severity::Critical, "D"),
            ],
            framework: "F".into(),
            had_sample: true,
        };
        let got: Vec<&str> = r.blocking().iter().map(|f| f.lint).collect();
        assert_eq!(got, vec!["crit", "high"]);
    }

    #[test]
    fn summary_line_counts_each_severity() {
        let r = LintReport {
            findings: vec![
                finding("a", Severity::High, "A"),
                finding("b", Severity::High, "B"),
                finding("c", Severity::Medium, "C"),
            ],
            framework: "F".into(),
            had_sample: true,
        };
        assert_eq!(r.summary_line(), "0 critical · 2 high · 1 medium · 0 low");
    }

    #[test]
    fn an_empty_report_is_clean() {
        let r = LintReport::default();
        assert!(r.is_clean());
        assert!(r.blocking().is_empty());
    }
}
