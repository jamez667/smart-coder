//! Cross-framework rollup — the facts an executive summary is built from.
//!
//! A per-framework report answers "how do we score against SOC 2". The question
//! a reader actually has is "what is wrong, how much does it matter, and what
//! should we do" — and the useful signal there is **which findings recur across
//! frameworks**. A single missing control that appears in six of ten frameworks
//! is one fix with six times the leverage, and no per-framework page can show
//! that.
//!
//! Everything here is computed deterministically from the audit results. The
//! optional narrative built on top ([`crate::narrative`]) is prose *about* these
//! facts; it never supplies facts of its own.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::evidence::EvidencePack;
use crate::status::{ControlStatus, Severity};

/// A finding that appears in more than one framework.
///
/// Grouped by *topic* rather than by exact check id — see [`topic_of`]. Pack
/// authors name the same underlying issue differently across frameworks, and
/// matching on the raw id would report one problem as several unrelated ones,
/// hiding exactly the leverage this type exists to surface.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecurringFinding {
    /// Human label for the topic, e.g. `"Automated secret detection"`.
    pub check: String,
    /// Frameworks in which it is a gap, by display name.
    pub frameworks: Vec<String>,
    /// The worst severity assigned to it by any framework.
    pub severity: Severity,
    /// One control's rationale for the check, as a human explanation.
    pub rationale: String,
    /// Remediation from the highest-severity control carrying it.
    pub remediation: Option<String>,
}

impl RecurringFinding {
    /// How many frameworks flag this.
    pub fn reach(&self) -> usize {
        self.frameworks.len()
    }
}

/// Totals and cross-framework analysis over every audited framework.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Rollup {
    pub frameworks: usize,
    pub controls: usize,
    pub passed: usize,
    pub gaps: usize,
    pub unknown: usize,
    pub errors: usize,
    /// Gap-causing checks, most widely-shared first.
    pub recurring: Vec<RecurringFinding>,
    /// Per-framework name and determinacy, worst first — where verification is
    /// thinnest.
    pub weakest_coverage: Vec<(String, f64)>,
    /// Capabilities switched off, deduplicated.
    pub disabled_capabilities: Vec<String>,
}

impl Rollup {
    /// Fraction of assessed controls that passed.
    pub fn pass_rate(&self) -> f64 {
        let scored = self.passed + self.gaps + self.unknown;
        if scored == 0 {
            return 0.0;
        }
        self.passed as f64 / scored as f64
    }

    /// Fraction that could be decided either way — the credibility of the above.
    pub fn determinacy(&self) -> f64 {
        let scored = self.passed + self.gaps + self.unknown;
        if scored == 0 {
            return 0.0;
        }
        (self.passed + self.gaps) as f64 / scored as f64
    }

    /// Findings that appear in more than one framework, highest leverage first.
    pub fn shared_findings(&self) -> Vec<&RecurringFinding> {
        self.recurring.iter().filter(|r| r.reach() > 1).collect()
    }

    /// Is there anything actionable at all?
    pub fn has_gaps(&self) -> bool {
        self.gaps > 0
    }
}

/// The local part of a `<control>/<check>` id.
fn local_check_id(qualified: &str) -> &str {
    qualified
        .split_once('/')
        .map(|(_, c)| c)
        .unwrap_or(qualified)
}

/// Group check ids that describe the same underlying issue.
///
/// Pack authors name the same control differently across frameworks —
/// `vulnerability-scanning-in-ci`, `dependency-audit-in-ci` and
/// `automated-update-policy` are three names for "nobody is watching our
/// dependencies". Correlating on the exact id would report them as three
/// unrelated single-framework findings and miss the one fact that matters:
/// it is ONE change that closes all of them.
///
/// Returns `(topic-key, human label)`. An id matching no topic keys on itself,
/// so a genuinely unique finding is still tracked.
fn topic_of(check_id: &str) -> (String, String) {
    const TOPICS: &[(&str, &str, &[&str])] = &[
        (
            "dependency-scanning",
            "Automated dependency vulnerability scanning",
            &[
                "vulnerability-scanning",
                "vulnerability-scanning-in-ci",
                "dependency-audit-in-ci",
                "automated-vulnerability-scanning",
                "patch-automation-configured",
                "automated-update-policy",
                "automated-remediation-policy",
                "automated-dependency-updates",
                "dependency-scanning-configured",
                "remediation-automation",
            ],
        ),
        (
            "secret-scanning",
            "Automated secret detection",
            &["secret-scanning-configured"],
        ),
        (
            "release-signing",
            "Signed releases and build provenance",
            &[
                "release-signing-configured",
                "provenance-signed",
                "provenance-generated",
                "checksum-publication",
                "trusted-builder-used",
            ],
        ),
        (
            "sbom",
            "Software bill of materials",
            &[
                "sbom-generated-in-ci",
                "sbom-generated",
                "sbom-artifact-present",
            ],
        ),
        (
            "static-analysis",
            "Static analysis in the pipeline",
            &[
                "static-analysis-in-ci",
                "static-analysis-configured",
                "automated-security-gate",
                "automated-code-review-tooling",
                "sast-tooling-in-ci",
            ],
        ),
        (
            "branch-protection",
            "Enforced review before merge",
            &[
                "review-required",
                "review-required-as-code",
                "pr-review-required",
                "branch-protection-as-code",
                "approval-enforced",
                "change-approval",
                "code-review-enforced-settings-yml",
            ],
        ),
        (
            "ci-testing",
            "Automated testing on every change",
            &[
                "ci-runs-tests",
                "tests-run-in-ci",
                "automated-testing-in-ci",
                "testing-before-release",
                "change-testing",
            ],
        ),
    ];

    for (key, label, ids) in TOPICS {
        if ids.contains(&check_id) {
            return ((*key).to_string(), (*label).to_string());
        }
    }
    (check_id.to_string(), check_id.replace('-', " "))
}

/// Build the rollup from every audited framework.
pub fn roll_up(packs: &[EvidencePack]) -> Rollup {
    let mut out = Rollup {
        frameworks: packs.len(),
        ..Default::default()
    };

    // check id -> (frameworks, worst severity, rationale, remediation)
    let mut recurring: BTreeMap<String, RecurringFinding> = BTreeMap::new();
    let mut caps: Vec<String> = Vec::new();

    for pack in packs {
        out.controls += pack.score.total;
        out.passed += pack.score.passed;
        out.gaps += pack.score.gaps;
        out.unknown += pack.score.unknown;
        out.errors += pack.score.errors;

        for cap in &pack.disabled_capabilities {
            if !caps.contains(cap) {
                caps.push(cap.clone());
            }
        }

        out.weakest_coverage
            .push((pack.framework.name.clone(), pack.score.determinacy()));

        for control in &pack.controls {
            if control.status != ControlStatus::Gap {
                continue;
            }
            for check in &control.checks {
                if check.status != ControlStatus::Gap {
                    continue;
                }
                // Group by TOPIC, not exact id: the same issue is named
                // differently in different packs, and reporting those as
                // unrelated findings hides the leverage.
                let (key, label) = topic_of(local_check_id(&check.check_id));
                let entry = recurring.entry(key).or_insert_with(|| RecurringFinding {
                    check: label,
                    frameworks: Vec::new(),
                    severity: control.severity,
                    rationale: check.rationale.trim().to_string(),
                    remediation: control.remediation.clone(),
                });
                if !entry.frameworks.contains(&pack.framework.name) {
                    entry.frameworks.push(pack.framework.name.clone());
                }
                // Keep the worst severity and the remediation that goes with it.
                if control.severity > entry.severity {
                    entry.severity = control.severity;
                    entry.remediation = control.remediation.clone();
                }
            }
        }
    }

    out.recurring = recurring.into_values().collect();
    // Widest reach first, then severity, then name — deterministic ordering so
    // a re-export does not reshuffle the page.
    out.recurring.sort_by(|a, b| {
        b.reach()
            .cmp(&a.reach())
            .then(b.severity.cmp(&a.severity))
            .then(a.check.cmp(&b.check))
    });

    out.weakest_coverage.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    out.disabled_capabilities = caps;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{CheckResult, ControlResult, FrameworkMeta};

    fn pack(name: &str, controls: Vec<ControlResult>) -> EvidencePack {
        EvidencePack::new(
            FrameworkMeta {
                id: name.to_lowercase(),
                name: name.to_string(),
                version: "1".into(),
                authority: "x".into(),
            },
            "(redacted)".into(),
            "t".into(),
            "scope".into(),
            controls,
            vec!["command-exit-code".into()],
        )
    }

    fn gap_control(id: &str, check: &str, sev: Severity) -> ControlResult {
        ControlResult {
            id: id.into(),
            title: "t".into(),
            section: Default::default(),
            clause: "c".into(),
            intent: "i".into(),
            severity: sev,
            status: ControlStatus::Gap,
            checks: vec![CheckResult {
                check_id: format!("{id}/{check}"),
                kind: "file-exists".into(),
                status: ControlStatus::Gap,
                weight: 1.0,
                evidence: vec![],
                note: None,
                rationale: "why this matters".into(),
            }],
            rationale: "r".into(),
            remediation: Some(format!("fix {check}")),
        }
    }

    fn passing_control(id: &str) -> ControlResult {
        ControlResult {
            status: ControlStatus::Pass,
            checks: vec![],
            ..gap_control(id, "x", Severity::Low)
        }
    }

    #[test]
    fn correlates_the_same_finding_across_frameworks() {
        // The whole point: one missing control appearing in three frameworks is
        // one fix with three times the leverage.
        let packs = vec![
            pack(
                "SOC 2",
                vec![gap_control("CC6.1", "secret-scanning", Severity::Critical)],
            ),
            pack(
                "ISO 27001",
                vec![gap_control("A.8.12", "secret-scanning", Severity::High)],
            ),
            pack(
                "PCI DSS",
                vec![gap_control("6.3", "secret-scanning", Severity::Medium)],
            ),
        ];
        let r = roll_up(&packs);

        let shared = r.shared_findings();
        assert_eq!(shared.len(), 1);
        // The field now carries a human LABEL, not the raw id.
        assert_eq!(shared[0].check, "secret scanning");
        assert_eq!(shared[0].reach(), 3);
        // The worst severity across frameworks wins, so priority is not
        // understated by whichever pack happened to be audited last.
        assert_eq!(shared[0].severity, Severity::Critical);
    }

    #[test]
    fn a_finding_in_one_framework_is_not_shared() {
        let packs = vec![
            pack(
                "SOC 2",
                vec![gap_control("CC6.1", "only-here", Severity::High)],
            ),
            pack("ISO 27001", vec![passing_control("A.1")]),
        ];
        let r = roll_up(&packs);
        assert!(r.shared_findings().is_empty());
        assert_eq!(r.recurring.len(), 1, "still recorded, just not shared");
    }

    #[test]
    fn recurring_is_ordered_by_reach_then_severity() {
        let packs = vec![
            pack(
                "A",
                vec![
                    gap_control("C1", "wide", Severity::Low),
                    gap_control("C2", "narrow-critical", Severity::Critical),
                ],
            ),
            pack("B", vec![gap_control("C3", "wide", Severity::Low)]),
        ];
        let r = roll_up(&packs);
        // Reach beats severity: a low-severity issue in two frameworks outranks
        // a critical one in a single framework for PRIORITISATION purposes.
        assert_eq!(r.recurring[0].check, "wide");
        assert_eq!(r.recurring[1].check, "narrow critical");
    }

    #[test]
    fn totals_sum_across_frameworks() {
        let packs = vec![
            pack(
                "A",
                vec![
                    passing_control("C1"),
                    gap_control("C2", "x", Severity::High),
                ],
            ),
            pack("B", vec![passing_control("C3")]),
        ];
        let r = roll_up(&packs);
        assert_eq!(r.frameworks, 2);
        assert_eq!(r.controls, 3);
        assert_eq!(r.passed, 2);
        assert_eq!(r.gaps, 1);
        assert!(r.has_gaps());
    }

    #[test]
    fn weakest_coverage_is_worst_first() {
        // Where verification is thinnest is the useful ordering — an exec should
        // see the least-verified framework first, not the alphabetically first.
        let strong = pack("Strong", vec![passing_control("C1"), passing_control("C2")]);
        let weak = pack(
            "Weak",
            vec![ControlResult {
                status: ControlStatus::Unknown,
                checks: vec![],
                ..gap_control("C3", "x", Severity::Low)
            }],
        );
        let r = roll_up(&[strong, weak]);
        assert_eq!(r.weakest_coverage[0].0, "Weak");
    }

    #[test]
    fn capabilities_are_deduplicated() {
        let packs = vec![pack("A", vec![]), pack("B", vec![])];
        let r = roll_up(&packs);
        assert_eq!(r.disabled_capabilities, vec!["command-exit-code"]);
    }

    #[test]
    fn an_empty_rollup_does_not_divide_by_zero() {
        let r = roll_up(&[]);
        assert!((r.pass_rate() - 0.0).abs() < f64::EPSILON);
        assert!((r.determinacy() - 0.0).abs() < f64::EPSILON);
        assert!(!r.has_gaps());
    }

    #[test]
    fn local_check_id_strips_the_control_prefix() {
        assert_eq!(local_check_id("CC6.1/secret-scanning"), "secret-scanning");
        assert_eq!(local_check_id("bare"), "bare");
    }

    #[test]
    fn differently_named_checks_for_one_issue_are_grouped() {
        // The real case this exists for: five pack authors named "nobody is
        // watching our dependencies" five different ways. Exact-id matching
        // reported five unrelated single-framework findings and hid the fact
        // that ONE change closes all of them.
        let packs = vec![
            pack(
                "SOC 2",
                vec![gap_control(
                    "CC7.1",
                    "dependency-audit-in-ci",
                    Severity::High,
                )],
            ),
            pack(
                "ISO 27001",
                vec![gap_control(
                    "A.8.8",
                    "vulnerability-scanning-in-ci",
                    Severity::High,
                )],
            ),
            pack(
                "800-53",
                vec![gap_control(
                    "SI-2",
                    "vulnerability-scanning",
                    Severity::High,
                )],
            ),
            pack(
                "SSDF",
                vec![gap_control(
                    "PW.4",
                    "automated-update-policy",
                    Severity::High,
                )],
            ),
        ];
        let r = roll_up(&packs);
        let shared = r.shared_findings();

        assert_eq!(
            shared.len(),
            1,
            "should be ONE issue, got {:?}",
            r.recurring
        );
        assert_eq!(shared[0].reach(), 4);
        assert!(
            shared[0].check.to_lowercase().contains("dependency"),
            "and it should be labelled readably, got {:?}",
            shared[0].check
        );
    }

    #[test]
    fn signing_and_provenance_group_together() {
        let packs = vec![
            pack(
                "SLSA",
                vec![gap_control("L2", "provenance-signed", Severity::High)],
            ),
            pack(
                "SSDF",
                vec![gap_control(
                    "PS.2",
                    "release-signing-configured",
                    Severity::High,
                )],
            ),
        ];
        assert_eq!(roll_up(&packs).shared_findings().len(), 1);
    }

    #[test]
    fn unrelated_findings_are_not_forced_together() {
        // The grouping must not over-merge: a secret-scanning gap and a
        // dependency gap are genuinely different fixes.
        let packs = vec![
            pack(
                "A",
                vec![gap_control(
                    "C1",
                    "secret-scanning-configured",
                    Severity::High,
                )],
            ),
            pack(
                "B",
                vec![gap_control("C2", "dependency-audit-in-ci", Severity::High)],
            ),
        ];
        let r = roll_up(&packs);
        assert!(
            r.shared_findings().is_empty(),
            "different issues, one framework each"
        );
        assert_eq!(r.recurring.len(), 2);
    }

    #[test]
    fn an_unknown_check_id_keys_on_itself() {
        let (key, label) = topic_of("something-nobody-mapped");
        assert_eq!(key, "something-nobody-mapped");
        assert_eq!(label, "something nobody mapped", "still readable");
    }
}
