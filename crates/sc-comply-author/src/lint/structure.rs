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

/// Words in a control's intent/title that mark it as organizational — settled by
/// interview, contract or record review rather than by reading code.
const ORG_MARKERS: &[&str] = &[
    "board",
    "oversight",
    "ethical",
    "integrity",
    "code of conduct",
    "vendor",
    "third-party risk",
    "business partner",
    "contract",
    "personnel",
    "onboarding",
    "offboarding",
    "background check",
    "training",
    "awareness",
    "incident response",
    "post-incident",
    "risk assessment",
    "management review",
    "internal audit",
    "physical access",
    "data center",
    "disposal",
    "insurance",
    "business continuity",
    "disaster recovery",
    "restore test",
];

/// Does this control describe something no repository can evidence?
fn looks_organizational(control: &Control) -> Option<&'static str> {
    let haystack = format!("{} {}", control.title, control.intent).to_lowercase();
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
