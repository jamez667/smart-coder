//! The status lattice, severity, and the outcome-mapping policy.
//!
//! This is the honesty layer of the crate. A compliance tool is either careful
//! here or it quietly lies everywhere else, so the types are deliberately
//! blunt: there are five statuses, `Unknown` is one of them, and nothing in the
//! crate is permitted to silently coerce it away.
//!
//! See `docs/specs/13-compliance-evidence.md`.

use serde::{Deserialize, Serialize};

/// The status of a single check, or of a whole control after aggregation.
///
/// The variant order is load-bearing: `Ord` is derived, so `all` aggregation is
/// literally `.max()` over this lattice — "worst wins".
///
/// Two orderings are worth justifying:
///
/// - `NotApplicable` is *lowest* so it can never drag a control down.
///   `Pass.max(NotApplicable) == Pass`.
/// - `Error` outranks `Gap` because a crashed collector means we don't know
///   whether there is a gap. Reporting "gap" when the tool broke is exactly as
///   wrong as reporting "pass".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlStatus {
    /// The control does not apply to this codebase. Excluded from scoring
    /// entirely — and specifically NOT counted as a pass. Counting N/A as a
    /// pass is the single most common way compliance dashboards inflate a
    /// score.
    NotApplicable,
    /// Evidence was found and it satisfies the control.
    Pass,
    /// We could not determine the answer from source.
    ///
    /// First-class and never silently coerced. "We didn't find it" and "it
    /// isn't there" are different claims with different legal weight: mapping
    /// this to `Pass` produces a false attestation, and mapping it to `Gap`
    /// produces the alert fatigue that gets the tool switched off. The set of
    /// `Unknown` controls IS the auditor's manual-evidence worklist.
    Unknown,
    /// Evidence was found and it violates the control, or required evidence is
    /// definitively absent.
    Gap,
    /// A collector failed. This is a *tool* failure, not a compliance
    /// judgment, and it is reported separately from findings.
    Error,
}

impl ControlStatus {
    /// A short label for tables and summaries.
    pub fn label(self) -> &'static str {
        match self {
            ControlStatus::NotApplicable => "n/a",
            ControlStatus::Pass => "pass",
            ControlStatus::Unknown => "unknown",
            ControlStatus::Gap => "gap",
            ControlStatus::Error => "error",
        }
    }

    /// Sort key for the report summary table: problems first, then things that
    /// need a human, then the good news. Never sort a compliance table by id —
    /// the reader wants the gaps at the top.
    pub fn report_order(self) -> u8 {
        match self {
            ControlStatus::Gap => 0,
            ControlStatus::Error => 1,
            ControlStatus::Unknown => 2,
            ControlStatus::Pass => 3,
            ControlStatus::NotApplicable => 4,
        }
    }

    /// Whether this status counts toward the in-scope denominator when scoring.
    pub fn is_in_scope(self) -> bool {
        self != ControlStatus::NotApplicable
    }
}

/// How serious a gap in a control is. Declared per-control in the pack and
/// inherited by every finding the control produces.
///
/// Ordered ascending so `.max()` over a control's findings yields the headline
/// severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// The SARIF `level` for this severity. SARIF has no "critical", so the top
    /// two both map to `error`.
    pub fn sarif_level(self) -> &'static str {
        match self {
            Severity::Critical | Severity::High => "error",
            Severity::Medium => "warning",
            Severity::Low => "note",
        }
    }
}

/// What a check outcome *means* for the control.
///
/// This is the vocabulary a pack author writes in `on_match` / `on_no_match` /
/// `on_no_files`. It is a deliberately narrower set than [`ControlStatus`]:
/// a pack can never declare an outcome to be `Error`, because `Error` is
/// reserved for the tool failing, and a pack author cannot know that in advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Pass,
    Gap,
    Unknown,
    NotApplicable,
}

impl From<Outcome> for ControlStatus {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Pass => ControlStatus::Pass,
            Outcome::Gap => ControlStatus::Gap,
            Outcome::Unknown => ControlStatus::Unknown,
            Outcome::NotApplicable => ControlStatus::NotApplicable,
        }
    }
}

/// The three-way outcome policy a check declares.
///
/// This is the heart of the pack format. Every check must say what a match, a
/// non-match, and *an inability to look* each mean — because those are three
/// genuinely different epistemic situations and collapsing them is how
/// compliance tools end up making claims they can't support.
///
/// `on_no_files` defaults to `on_no_match` (see [`OutcomePolicy::resolve`]),
/// but wherever "couldn't look" differs from "looked and it wasn't there", the
/// pack author sets it explicitly. The canonical example is branch protection:
/// it lives in the VCS provider's API, not the repo, so the absence of a
/// settings file is emphatically *not* evidence that review isn't required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct OutcomePolicy {
    pub on_match: Outcome,
    pub on_no_match: Outcome,
    /// `None` means "same as `on_no_match`".
    pub on_no_files: Option<Outcome>,
}

impl OutcomePolicy {
    /// Map a collector's raw observation through this policy.
    ///
    /// `matched == None` is the "could not determine" case: no file matched the
    /// glob, the file was unparseable, or the capability was disabled.
    pub fn resolve(&self, matched: Option<bool>) -> ControlStatus {
        match matched {
            Some(true) => self.on_match.into(),
            Some(false) => self.on_no_match.into(),
            None => self.on_no_files.unwrap_or(self.on_no_match).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_aggregation_is_max_over_the_lattice() {
        // Worst wins, and N/A never drags anything down.
        assert_eq!(
            ControlStatus::Pass.max(ControlStatus::NotApplicable),
            ControlStatus::Pass
        );
        assert_eq!(
            ControlStatus::Pass.max(ControlStatus::Unknown),
            ControlStatus::Unknown
        );
        assert_eq!(
            ControlStatus::Unknown.max(ControlStatus::Gap),
            ControlStatus::Gap
        );
        assert_eq!(
            ControlStatus::Gap.max(ControlStatus::Error),
            ControlStatus::Error
        );
    }

    #[test]
    fn error_outranks_gap() {
        // A crashed collector must never be reported as a compliance gap.
        assert!(ControlStatus::Error > ControlStatus::Gap);
    }

    #[test]
    fn not_applicable_is_lowest() {
        assert!(ControlStatus::NotApplicable < ControlStatus::Pass);
        assert!(!ControlStatus::NotApplicable.is_in_scope());
        assert!(ControlStatus::Unknown.is_in_scope());
    }

    #[test]
    fn report_order_puts_problems_first() {
        let mut v = vec![
            ControlStatus::Pass,
            ControlStatus::NotApplicable,
            ControlStatus::Gap,
            ControlStatus::Unknown,
            ControlStatus::Error,
        ];
        v.sort_by_key(|s| s.report_order());
        assert_eq!(
            v,
            vec![
                ControlStatus::Gap,
                ControlStatus::Error,
                ControlStatus::Unknown,
                ControlStatus::Pass,
                ControlStatus::NotApplicable,
            ]
        );
    }

    fn policy(
        on_match: Outcome,
        on_no_match: Outcome,
        on_no_files: Option<Outcome>,
    ) -> OutcomePolicy {
        OutcomePolicy {
            on_match,
            on_no_match,
            on_no_files,
        }
    }

    #[test]
    fn outcome_mapping_table() {
        let p = policy(Outcome::Pass, Outcome::Gap, Some(Outcome::Unknown));
        assert_eq!(p.resolve(Some(true)), ControlStatus::Pass);
        assert_eq!(p.resolve(Some(false)), ControlStatus::Gap);
        assert_eq!(p.resolve(None), ControlStatus::Unknown);
    }

    #[test]
    fn on_no_files_defaults_to_on_no_match() {
        let p = policy(Outcome::Pass, Outcome::Gap, None);
        assert_eq!(p.resolve(None), ControlStatus::Gap);
    }

    #[test]
    fn a_must_not_match_check_inverts_cleanly() {
        // regex-must-not-match: a hit is the bad outcome.
        let p = policy(Outcome::Gap, Outcome::Pass, None);
        assert_eq!(p.resolve(Some(true)), ControlStatus::Gap);
        assert_eq!(p.resolve(Some(false)), ControlStatus::Pass);
    }

    #[test]
    fn severity_orders_ascending_and_maps_to_sarif() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert_eq!(Severity::Critical.sarif_level(), "error");
        assert_eq!(Severity::High.sarif_level(), "error");
        assert_eq!(Severity::Medium.sarif_level(), "warning");
        assert_eq!(Severity::Low.sarif_level(), "note");
    }
}
