//! How check results roll up into a control status.
//!
//! This is where compliance tools are either honest or quietly wrong, so every
//! rule here is stated explicitly and tested individually. The recurring theme:
//! *partial evidence is not a verdict*. A control we could only half-observe is
//! `Unknown`, not `Gap` and certainly not `Pass`.
//!
//! See `docs/specs/13-compliance-evidence.md`.

use serde::Deserialize;

use crate::evidence::CheckResult;
use crate::status::ControlStatus;

/// How a control's check results combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Aggregate {
    /// Conjunctive: every check must hold. Worst status wins. The default.
    #[default]
    All,
    /// Disjunctive: any one acceptable mechanism is sufficient.
    Any,
    /// Fraction of observable weight, against `pass_at` / `gap_below`.
    Weighted,
    /// More than half the scoring checks pass, and gaps do not dominate.
    Majority,
}

/// Thresholds for [`Aggregate::Weighted`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeightCfg {
    /// Ratio at or above which the control passes.
    pub pass_at: f64,
    /// Ratio below which the control is a gap. Between the two is the
    /// partial-evidence band, which resolves to `Unknown`.
    pub gap_below: f64,
    /// If more than this share of the control's weight was indeterminate, the
    /// ratio is noise and the whole control is `Unknown`.
    pub max_unknown_share: f64,
}

impl Default for WeightCfg {
    fn default() -> Self {
        WeightCfg {
            pass_at: 0.75,
            gap_below: 0.40,
            max_unknown_share: 0.5,
        }
    }
}

/// Roll a control's check results up into a single status plus the human
/// explanation of how it was derived.
///
/// The returned rationale is not decoration — it goes verbatim into the report
/// so an auditor can see the arithmetic without asking.
pub fn aggregate(
    agg: Aggregate,
    results: &[CheckResult],
    cfg: &WeightCfg,
) -> (ControlStatus, String) {
    // Rule 1: a tool failure dominates every aggregate, including `Any`.
    //
    // This is the rule most tools get wrong: they treat a crashed scanner as a
    // pass. A control cannot be declared satisfied on partial evidence when the
    // collector that might have found the gap is precisely the one that broke.
    if results.iter().any(|r| r.status == ControlStatus::Error) {
        let n = results
            .iter()
            .filter(|r| r.status == ControlStatus::Error)
            .count();
        return (
            ControlStatus::Error,
            format!("{n} collector(s) failed; control status is indeterminate"),
        );
    }

    // Rule 2: N/A checks leave the scoring set entirely.
    let scoring: Vec<&CheckResult> = results
        .iter()
        .filter(|r| r.status != ControlStatus::NotApplicable)
        .collect();

    if scoring.is_empty() {
        // Either there were no checks at all (rejected at pack load) or every
        // one was inapplicable to this codebase.
        return (
            ControlStatus::NotApplicable,
            "no applicable checks for this codebase".to_string(),
        );
    }

    match agg {
        Aggregate::All => {
            // Worst wins — exactly `Ord::max` over the lattice.
            let st = scoring
                .iter()
                .map(|r| r.status)
                .max()
                .unwrap_or(ControlStatus::Unknown);
            let n = scoring.len();
            (
                st,
                format!("all-of: worst of {n} check(s) is {}", st.label()),
            )
        }

        Aggregate::Any => {
            let passes = scoring
                .iter()
                .filter(|r| r.status == ControlStatus::Pass)
                .count();
            if passes > 0 {
                return (
                    ControlStatus::Pass,
                    format!(
                        "any-of: {passes} of {} mechanism(s) evidenced",
                        scoring.len()
                    ),
                );
            }
            // No mechanism found. That is only a *gap* if we could actually
            // evaluate every alternative. If a pack lists three acceptable TLS
            // mechanisms and we could only see one, "no mechanism found" is not
            // a defensible claim.
            if scoring.iter().all(|r| r.status == ControlStatus::Gap) {
                (
                    ControlStatus::Gap,
                    format!("any-of: none of {} mechanism(s) evidenced", scoring.len()),
                )
            } else {
                let unk = scoring
                    .iter()
                    .filter(|r| r.status == ControlStatus::Unknown)
                    .count();
                (
                    ControlStatus::Unknown,
                    format!(
                        "any-of: no mechanism evidenced, and {unk} of {} check(s) were indeterminate",
                        scoring.len()
                    ),
                )
            }
        }

        Aggregate::Weighted => {
            let earned: f64 = scoring
                .iter()
                .filter(|r| r.status == ControlStatus::Pass)
                .map(|r| r.weight)
                .sum();
            let unknown_w: f64 = scoring
                .iter()
                .filter(|r| r.status == ControlStatus::Unknown)
                .map(|r| r.weight)
                .sum();
            // The denominator is what we could OBSERVE, not the total. Dividing
            // by total weight would penalize the codebase for the tool's own
            // blind spots.
            let observable: f64 = scoring
                .iter()
                .filter(|r| r.status != ControlStatus::Unknown)
                .map(|r| r.weight)
                .sum();
            let total = observable + unknown_w;

            // ...and then, separately, ask whether we saw enough to have an
            // opinion at all. Two mechanisms, two distinct questions.
            if total <= f64::EPSILON || unknown_w / total > cfg.max_unknown_share {
                let pct = if total > f64::EPSILON {
                    unknown_w / total * 100.0
                } else {
                    100.0
                };
                return (
                    ControlStatus::Unknown,
                    format!("weighted: {pct:.0}% of evidence weight was indeterminate"),
                );
            }

            let ratio = earned / observable;
            let st = if ratio >= cfg.pass_at {
                ControlStatus::Pass
            } else if ratio < cfg.gap_below {
                ControlStatus::Gap
            } else {
                // The partial-evidence band. Some of the control is evidenced
                // but not enough to attest — that means "an auditor should look
                // at this", not "this failed".
                ControlStatus::Unknown
            };
            (
                st,
                format!(
                    "weighted: {earned:.1}/{observable:.1} observable = {:.0}% (pass>={:.0}%, gap<{:.0}%)",
                    ratio * 100.0,
                    cfg.pass_at * 100.0,
                    cfg.gap_below * 100.0
                ),
            )
        }

        Aggregate::Majority => {
            let passes = scoring
                .iter()
                .filter(|r| r.status == ControlStatus::Pass)
                .count();
            let gaps = scoring
                .iter()
                .filter(|r| r.status == ControlStatus::Gap)
                .count();
            let n = scoring.len();
            let st = if gaps > 0 && passes <= gaps {
                ControlStatus::Gap
            } else if passes * 2 > n {
                ControlStatus::Pass
            } else {
                ControlStatus::Unknown
            };
            (
                st,
                format!("majority: {passes} pass / {gaps} gap of {n} check(s)"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: ControlStatus, weight: f64) -> CheckResult {
        CheckResult {
            check_id: "c".into(),
            kind: "file-exists".into(),
            status,
            weight,
            evidence: vec![],
            note: None,
            rationale: "r".into(),
        }
    }

    const ALL_AGGREGATES: [Aggregate; 4] = [
        Aggregate::All,
        Aggregate::Any,
        Aggregate::Weighted,
        Aggregate::Majority,
    ];

    #[test]
    fn error_dominates_every_aggregate() {
        // Even alongside a pass, and even under `Any`, which would otherwise
        // be satisfied by that pass.
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Error, 1.0),
        ];
        for agg in ALL_AGGREGATES {
            let (st, _) = aggregate(agg, &results, &WeightCfg::default());
            assert_eq!(
                st,
                ControlStatus::Error,
                "aggregate {agg:?} let an Error through"
            );
        }
    }

    #[test]
    fn all_takes_worst_status() {
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Unknown, 1.0),
            check(ControlStatus::Gap, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::All, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Gap);
    }

    #[test]
    fn not_applicable_never_drags_down_all() {
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::NotApplicable, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::All, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Pass);
    }

    #[test]
    fn all_checks_not_applicable_makes_control_not_applicable() {
        let results = vec![
            check(ControlStatus::NotApplicable, 1.0),
            check(ControlStatus::NotApplicable, 1.0),
        ];
        for agg in ALL_AGGREGATES {
            let (st, _) = aggregate(agg, &results, &WeightCfg::default());
            assert_eq!(st, ControlStatus::NotApplicable, "aggregate {agg:?}");
        }
    }

    #[test]
    fn empty_results_are_not_applicable_not_pass() {
        // Defensive: a control with no checks is rejected at pack load, but if
        // one ever reaches here it must not vacuously "pass".
        for agg in ALL_AGGREGATES {
            let (st, _) = aggregate(agg, &[], &WeightCfg::default());
            assert_eq!(st, ControlStatus::NotApplicable, "aggregate {agg:?}");
        }
    }

    #[test]
    fn any_passes_on_a_single_mechanism() {
        let results = vec![
            check(ControlStatus::Gap, 1.0),
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Unknown, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Any, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Pass);
    }

    #[test]
    fn any_with_all_gaps_is_a_gap() {
        let results = vec![
            check(ControlStatus::Gap, 1.0),
            check(ControlStatus::Gap, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Any, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Gap);
    }

    #[test]
    fn any_with_one_unknown_and_rest_gap_is_unknown_not_gap() {
        // The key `Any` rule: we cannot claim "no acceptable mechanism exists"
        // when one of the alternatives was never actually evaluated.
        let results = vec![
            check(ControlStatus::Gap, 1.0),
            check(ControlStatus::Gap, 1.0),
            check(ControlStatus::Unknown, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Any, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Unknown);
    }

    #[test]
    fn weighted_denominator_excludes_unknown_weight() {
        // 3.0 earned, 1.0 gap, 4.0 unknown. Against total (8.0) the ratio is
        // 0.375 -> Gap. Against observable (4.0) it is 0.75 -> Pass.
        // Unknown weight is exactly at the 0.5 share limit, so it does not veto.
        let results = vec![
            check(ControlStatus::Pass, 3.0),
            check(ControlStatus::Gap, 1.0),
            check(ControlStatus::Unknown, 4.0),
        ];
        let (st, why) = aggregate(Aggregate::Weighted, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Pass, "{why}");
        assert!(why.contains("3.0/4.0"), "{why}");
    }

    #[test]
    fn weighted_vetoes_on_excessive_unknown_share() {
        // 1.0 observable against 9.0 unknown: 90% indeterminate. Even though
        // everything observable passed, we have no basis for an opinion.
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Unknown, 9.0),
        ];
        let (st, why) = aggregate(Aggregate::Weighted, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Unknown);
        assert!(why.contains("90%"), "{why}");
    }

    #[test]
    fn weighted_middle_band_is_unknown_not_gap() {
        // 5.0 of 10.0 observable = 50%: below pass_at (75%) but at/above
        // gap_below (40%). Partial evidence means "look at this", not "failed".
        let results = vec![
            check(ControlStatus::Pass, 5.0),
            check(ControlStatus::Gap, 5.0),
        ];
        let (st, _) = aggregate(Aggregate::Weighted, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Unknown);
    }

    #[test]
    fn weighted_below_gap_threshold_is_a_gap() {
        // 1.0 of 10.0 = 10%, well under gap_below.
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Gap, 9.0),
        ];
        let (st, _) = aggregate(Aggregate::Weighted, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Gap);
    }

    #[test]
    fn weighted_all_observable_passing_is_a_pass() {
        let results = vec![
            check(ControlStatus::Pass, 2.0),
            check(ControlStatus::Pass, 3.0),
        ];
        let (st, _) = aggregate(Aggregate::Weighted, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Pass);
    }

    #[test]
    fn weighted_all_unknown_is_unknown_not_a_divide_by_zero() {
        let results = vec![
            check(ControlStatus::Unknown, 1.0),
            check(ControlStatus::Unknown, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Weighted, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Unknown);
    }

    #[test]
    fn weighted_respects_custom_thresholds() {
        // 5.0/10.0 = 50%. Under the default it is the middle band (Unknown),
        // but a lenient pack can declare that a pass.
        let results = vec![
            check(ControlStatus::Pass, 5.0),
            check(ControlStatus::Gap, 5.0),
        ];
        let lenient = WeightCfg {
            pass_at: 0.5,
            gap_below: 0.25,
            max_unknown_share: 0.5,
        };
        let (st, _) = aggregate(Aggregate::Weighted, &results, &lenient);
        assert_eq!(st, ControlStatus::Pass);
    }

    #[test]
    fn majority_gap_beats_tie() {
        // 1 pass, 1 gap: a tie is not a majority, and the presence of a gap
        // with no majority of passes means Gap.
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Gap, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Majority, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Gap);
    }

    #[test]
    fn majority_passes_with_more_than_half() {
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Unknown, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Majority, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Pass);
    }

    #[test]
    fn majority_without_majority_or_gaps_is_unknown() {
        let results = vec![
            check(ControlStatus::Pass, 1.0),
            check(ControlStatus::Unknown, 1.0),
            check(ControlStatus::Unknown, 1.0),
        ];
        let (st, _) = aggregate(Aggregate::Majority, &results, &WeightCfg::default());
        assert_eq!(st, ControlStatus::Unknown);
    }

    #[test]
    fn no_aggregate_ever_invents_a_pass_from_unknowns() {
        // The single most important safety property in the crate: if nothing
        // was actually observed to pass, no aggregation rule may return Pass.
        let results = vec![
            check(ControlStatus::Unknown, 1.0),
            check(ControlStatus::Unknown, 5.0),
            check(ControlStatus::NotApplicable, 1.0),
        ];
        for agg in ALL_AGGREGATES {
            let (st, _) = aggregate(agg, &results, &WeightCfg::default());
            assert_ne!(st, ControlStatus::Pass, "aggregate {agg:?} invented a pass");
        }
    }

    #[test]
    fn rationale_is_always_populated() {
        for agg in ALL_AGGREGATES {
            let (_, why) = aggregate(
                agg,
                &[check(ControlStatus::Pass, 1.0)],
                &WeightCfg::default(),
            );
            assert!(!why.is_empty(), "aggregate {agg:?} produced no rationale");
        }
    }
}
