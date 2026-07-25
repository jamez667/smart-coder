//! Control-level structure: thresholds, severity hygiene, and the big one —
//! controls that claim more determinism than source inspection can deliver.
//!
//! The last of those is the most important lint in the crate after the
//! `on_no_files` family. Roughly 85% of a framework like SOC 2 is organizational:
//! board oversight, vendor contracts, incident records, access reviews. None of
//! it is visible in a repository. A pack that lets such a control resolve to
//! `pass` because it found a Markdown file has confused *documented* with
//! *operating*, and that is precisely the error that turns an evidence pack into
//! a false attestation.

use sc_comply::aggregate::Aggregate;
use sc_comply::pack::Control;
use sc_comply::status::{Outcome, Severity};

use super::{LintCtx, LintFinding};

/// Phrases marking a control as organizational — settled by interview, contract
/// or record review rather than by reading code.
///
/// These are deliberately **phrases, not bare words**. An earlier version listed
/// `"personnel"`, `"integrity"`, `"contract"` and `"training"` on their own and
/// fired on NIST SSDF PS.1 ("only authorized personnel can change it") and PS.2
/// ("software acquirers can verify integrity") — both squarely technical
/// controls about branch protection and release signing. A lint that cries wolf
/// on real controls gets switched off, which costs more than the misses.
const ORG_MARKERS: &[&str] = &[
    "board of directors",
    "board oversight",
    "management approval",
    "approved by management",
    "management review",
    "ethical values",
    "code of conduct",
    "vendor",
    "supplier relationship",
    "third-party risk",
    "business partner",
    "due diligence",
    "personnel shall receive",
    "acknowledged by relevant personnel",
    "onboarding",
    "offboarding",
    "background check",
    "awareness, education and training",
    "security awareness",
    "training completion",
    "incident response programme",
    "incident response plan",
    "post-incident",
    "risk assessment",
    "internal audit",
    "physical access",
    "physical security",
    "data center",
    "secure disposal",
    "insurance",
    "business continuity",
    "disaster recovery",
    "restore test",
    "acknowledgement record",
];

/// Topics that make a control technical even if an org phrase also appears.
///
/// A control can legitimately mention people while being about a mechanism —
/// "only authorized personnel can change the code" is enforced by branch
/// protection, which a repository absolutely can evidence.
const TECHNICAL_OVERRIDES: &[&str] = &[
    "branch protection",
    "least privilege",
    "signing",
    "signature",
    "checksum",
    "encryption",
    "cryptograph",
    "sbom",
    "provenance",
    "dependency",
    "vulnerability scan",
    "static analysis",
    "secure coding",
    "logging",
    "pipeline",
    "unauthorized access and tampering",
    "release integrity",
];

/// Does this control describe something no repository can evidence?
///
/// Returns the matching phrase, or `None` when a technical topic overrides it.
fn looks_organizational(control: &Control) -> Option<&'static str> {
    let haystack = format!("{} {}", control.title, control.intent).to_lowercase();
    if TECHNICAL_OVERRIDES.iter().any(|t| haystack.contains(t)) {
        return None;
    }
    ORG_MARKERS.iter().copied().find(|m| haystack.contains(m))
}

/// Can any check in this control resolve to `pass`?
fn can_pass(control: &Control) -> bool {
    control.checks.iter().any(|k| {
        k.on_match == Outcome::Pass
            || k.on_no_match == Outcome::Pass
            || k.on_no_files.unwrap_or(k.on_no_match) == Outcome::Pass
    })
}

pub fn run(ctx: &LintCtx<'_>, out: &mut Vec<LintFinding>) {
    for control in &ctx.pack.controls {
        org_control_claims_determinism(control, out);
        weighted_thresholds_implausible(control, out);
        severity_without_remediation(control, out);
        any_of_single_check(control, out);
        missing_intent(control, out);
    }
}

/// An organizational control that can nonetheless resolve to `pass`.
fn org_control_claims_determinism(control: &Control, out: &mut Vec<LintFinding>) {
    let Some(marker) = looks_organizational(control) else {
        return;
    };
    if !can_pass(control) {
        return;
    }
    out.push(LintFinding::new(
        "org-control-claims-determinism",
        Severity::High,
        &control.id,
        format!(
            "the control reads as organizational (mentions {marker:?}) but a check can still resolve to `pass`"
        ),
        "Finding a policy document evidences that something is DOCUMENTED, never that it OPERATES. A green control here tells an auditor a process was verified when only a file was found.".to_string(),
        "Map the outcomes to `unknown` and state the manual step in the rationale (e.g. \"auditor must obtain acknowledgement records\"). Declaring the control Unknown is honest; omitting it entirely would imply the framework was fully covered.",
    ));
}

/// Weighted thresholds that make the control effectively binary, or leave a band
/// so wide that almost nothing can ever be decided.
fn weighted_thresholds_implausible(control: &Control, out: &mut Vec<LintFinding>) {
    if control.aggregate != Aggregate::Weighted {
        return;
    }
    let cfg = control.weight_cfg();
    let band = cfg.pass_at - cfg.gap_below;

    if band < 0.05 {
        out.push(LintFinding::new(
            "weighted-band-too-narrow",
            Severity::Low,
            &control.id,
            format!(
                "pass_at ({:.2}) and gap_below ({:.2}) leave almost no partial-evidence band",
                cfg.pass_at, cfg.gap_below
            ),
            "The control is effectively binary, so partial evidence resolves straight to pass or gap instead of the Unknown that says \"an auditor should look at this\".".to_string(),
            "Widen the gap between the thresholds, or switch to `aggregate = \"all\"` if the control really is all-or-nothing.",
        ));
    } else if band > 0.6 {
        out.push(LintFinding::new(
            "weighted-band-too-wide",
            Severity::Low,
            &control.id,
            format!(
                "pass_at ({:.2}) and gap_below ({:.2}) leave a {:.0}% band that resolves to Unknown",
                cfg.pass_at,
                cfg.gap_below,
                band * 100.0
            ),
            "Most evidence ratios will land in the middle band, so the control almost always reports Unknown and adds nothing but noise to the worklist.".to_string(),
            "Tighten the thresholds so a well-evidenced repo can actually pass.",
        ));
    }
}

/// A serious control with no remediation guidance.
fn severity_without_remediation(control: &Control, out: &mut Vec<LintFinding>) {
    if control.severity < Severity::High {
        return;
    }
    if control
        .remediation
        .as_ref()
        .is_some_and(|r| !r.trim().is_empty())
    {
        return;
    }
    out.push(LintFinding::new(
        "severity-without-remediation",
        Severity::Low,
        &control.id,
        format!(
            "{:?}-severity control has no `remediation`",
            control.severity
        ),
        "The gap section will tell a reader something is seriously wrong without telling them what to do, which is where a report stops being actionable.".to_string(),
        "Add a `remediation` naming the concrete fix. For an Unknown-by-design control, say what document the auditor must obtain instead.",
    ));
}

/// `aggregate = "any"` with exactly one check.
fn any_of_single_check(control: &Control, out: &mut Vec<LintFinding>) {
    if control.aggregate != Aggregate::Any || control.checks.len() != 1 {
        return;
    }
    out.push(LintFinding::new(
        "any-of-single-check",
        Severity::Low,
        &control.id,
        "`aggregate = \"any\"` with only one check behaves differently from `all` in a way that is easy to miss",
        "With one check, `any` cannot return Gap unless that check is a Gap — but the Unknown-handling differs subtly from `all`, so the choice looks accidental.".to_string(),
        "Use `aggregate = \"all\"` for a single check, or add the alternative mechanisms the `any` was written for.",
    ));
}

/// A control with no `intent`.
fn missing_intent(control: &Control, out: &mut Vec<LintFinding>) {
    if !control.intent.trim().is_empty() {
        return;
    }
    out.push(LintFinding::new(
        "missing-intent",
        Severity::Low,
        &control.id,
        "the control has no `intent`",
        "The report renders intent verbatim so a reader knows what the auditor is actually asking. Without it, a finding is a bare id with no context.".to_string(),
        "Add an `intent` paraphrasing the framework's own wording for this control.",
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint::lint_pack;

    fn findings(pack_src: &str, lint: &'static str) -> Vec<LintFinding> {
        let pack = sc_comply::Pack::from_toml_str(pack_src).expect("pack parses");
        lint_pack(&pack, None)
            .findings
            .into_iter()
            .filter(|f| f.lint == lint)
            .collect()
    }

    /// A control with explicit fields, so structural lints can be targeted.
    fn control_pack(header: &str, checks: &str) -> String {
        format!(
            r#"
[framework]
id = "t"
name = "T"
version = "1"
authority = "A"

[[controls]]
{header}
{checks}
"#
        )
    }

    const PASSING_CHECK: &str = r#"
  [[controls.checks]]
  id = "doc"
  kind = "file-exists"
  paths = ["CODE_OF_CONDUCT.md"]
  on_match = "pass"
  on_no_match = "gap"
"#;

    const UNKNOWN_CHECK: &str = r#"
  [[controls.checks]]
  id = "doc"
  kind = "file-exists"
  paths = ["CODE_OF_CONDUCT.md"]
  on_match = "unknown"
  on_no_match = "unknown"
"#;

    #[test]
    fn flags_an_org_control_that_can_pass() {
        // The "found CODE_OF_CONDUCT.md therefore compliant" error.
        let src = control_pack(
            r#"id = "CC1.1"
title = "Commitment to integrity and ethical values"
intent = "The entity demonstrates a commitment to integrity, with board oversight."
severity = "medium""#,
            PASSING_CHECK,
        );
        let found = findings(&src, "org-control-claims-determinism");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::High);
    }

    #[test]
    fn accepts_an_org_control_that_only_yields_unknown() {
        // How the shipped SOC 2 pack declares CC1.1. Must not be flagged.
        let src = control_pack(
            r#"id = "CC1.1"
title = "Commitment to integrity and ethical values"
intent = "The entity demonstrates a commitment to integrity, with board oversight."
severity = "medium""#,
            UNKNOWN_CHECK,
        );
        assert!(findings(&src, "org-control-claims-determinism").is_empty());
    }

    #[test]
    fn a_technical_control_mentioning_people_is_not_flagged() {
        // Regression: NIST SSDF PS.1 says "only authorized personnel can change
        // it", which is branch protection — squarely source-evidenceable. An
        // earlier marker list contained the bare word "personnel" and fired on
        // it, which is exactly how a linter earns being ignored.
        let src = control_pack(
            r#"id = "PS.1"
title = "Protect all forms of code from unauthorized access and tampering"
intent = "Store all forms of code based on least privilege so only authorized personnel can change it."
severity = "high"
remediation = "Enforce branch protection." "#,
            PASSING_CHECK,
        );
        assert!(
            findings(&src, "org-control-claims-determinism").is_empty(),
            "PS.1 is a technical control about branch protection"
        );
    }

    #[test]
    fn a_release_integrity_control_is_not_flagged() {
        // Regression: SSDF PS.2 mentions "acquirers" and "integrity" but is
        // about release signing.
        let src = control_pack(
            r#"id = "PS.2"
title = "Provide a mechanism for verifying software release integrity"
intent = "Make verification information available so acquirers can confirm the software is authentic and unmodified."
severity = "high"
remediation = "Sign releases with cosign." "#,
            PASSING_CHECK,
        );
        assert!(findings(&src, "org-control-claims-determinism").is_empty());
    }

    #[test]
    fn a_genuinely_organizational_control_is_still_flagged() {
        // The narrowing must not blunt the lint on real cases.
        for (id, title, intent) in [
            (
                "A.6.3",
                "Information security awareness, education and training",
                "Personnel shall receive appropriate security awareness, education and training.",
            ),
            (
                "CC9.2",
                "Vendor risk management",
                "The entity performs due diligence on each vendor before engagement.",
            ),
            (
                "A.7.1",
                "Physical security perimeters",
                "Physical security perimeters shall protect areas containing information assets.",
            ),
        ] {
            let src = control_pack(
                &format!(
                    "id = \"{id}\"\ntitle = \"{title}\"\nintent = \"{intent}\"\nseverity = \"medium\""
                ),
                PASSING_CHECK,
            );
            assert_eq!(
                findings(&src, "org-control-claims-determinism").len(),
                1,
                "{id} should still be flagged"
            );
        }
    }

    #[test]
    fn a_technical_control_that_passes_is_not_flagged() {
        // The false-positive guard that matters most here: normal technical
        // controls must pass silently.
        let src = control_pack(
            r#"id = "CC6.1"
title = "Logical access - credentials are not committed"
intent = "Committed credentials defeat every downstream access control."
severity = "critical"
remediation = "Rotate the credential and add a secret scanner." "#,
            r#"
  [[controls.checks]]
  id = "keys"
  kind = "regex-must-not-match"
  glob = "**/*.rs"
  pattern = "BEGIN RSA PRIVATE KEY"
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown"
"#,
        );
        assert!(findings(&src, "org-control-claims-determinism").is_empty());
    }

    #[test]
    fn flags_a_narrow_weighted_band() {
        let src = control_pack(
            r#"id = "W1"
title = "t"
intent = "i"
aggregate = "weighted"
pass_at = 0.52
gap_below = 0.50"#,
            PASSING_CHECK,
        );
        assert_eq!(findings(&src, "weighted-band-too-narrow").len(), 1);
    }

    #[test]
    fn flags_a_very_wide_weighted_band() {
        let src = control_pack(
            r#"id = "W2"
title = "t"
intent = "i"
aggregate = "weighted"
pass_at = 0.95
gap_below = 0.05"#,
            PASSING_CHECK,
        );
        assert_eq!(findings(&src, "weighted-band-too-wide").len(), 1);
    }

    #[test]
    fn accepts_the_default_weighted_thresholds() {
        // 0.75 / 0.40 — what the shipped CC8.1 uses.
        let src = control_pack(
            r#"id = "W3"
title = "t"
intent = "i"
aggregate = "weighted"
pass_at = 0.75
gap_below = 0.40"#,
            PASSING_CHECK,
        );
        assert!(findings(&src, "weighted-band-too-narrow").is_empty());
        assert!(findings(&src, "weighted-band-too-wide").is_empty());
    }

    #[test]
    fn flags_high_severity_without_remediation() {
        let src = control_pack(
            r#"id = "S1"
title = "t"
intent = "i"
severity = "critical""#,
            PASSING_CHECK,
        );
        assert_eq!(findings(&src, "severity-without-remediation").len(), 1);
    }

    #[test]
    fn low_severity_needs_no_remediation() {
        let src = control_pack(
            r#"id = "S2"
title = "t"
intent = "i"
severity = "low""#,
            PASSING_CHECK,
        );
        assert!(findings(&src, "severity-without-remediation").is_empty());
    }

    #[test]
    fn flags_any_of_with_one_check() {
        let src = control_pack(
            r#"id = "A1"
title = "t"
intent = "i"
aggregate = "any""#,
            PASSING_CHECK,
        );
        assert_eq!(findings(&src, "any-of-single-check").len(), 1);
    }

    #[test]
    fn flags_a_missing_intent() {
        let src = control_pack(
            r#"id = "M1"
title = "t""#,
            PASSING_CHECK,
        );
        assert_eq!(findings(&src, "missing-intent").len(), 1);
    }
}
