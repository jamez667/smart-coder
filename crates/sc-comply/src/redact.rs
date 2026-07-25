//! Redaction for publication.
//!
//! An evidence pack is written for an auditor who is *entitled* to see where the
//! problems are. Publishing one changes the audience: `file:line` citations, the
//! excerpts around them, and the detection patterns in the manifest together
//! form a map of a codebase's weaknesses that anyone can read, permanently and
//! indexed.
//!
//! # The redaction is structural, not cosmetic
//!
//! [`EvidencePack::redacted`] returns a **new pack with the sensitive fields
//! removed**, rather than a flag that renderers are trusted to honour. That
//! distinction is the whole safety argument: a renderer added next year cannot
//! leak what is no longer in the data structure it was handed. A `redacted:
//! bool` on the pack would put one forgotten `if` between an internal audit and
//! a public one.
//!
//! # What survives
//!
//! Everything that demonstrates judgment without mapping the attack surface:
//! control statuses, the aggregation rationale, scope notes, intents, check
//! rationales, and the counts. A reader learns *that* CC6.1 has a gap and *why
//! it matters*; they do not learn which file to open.

use crate::evidence::{CheckResult, ControlResult, EvidencePack};

/// The marker a redacted note carries, so redaction can recognise its own work.
const WITHHELD: &str = "citation(s) withheld from the published report";

/// Redact one check: drop the citations, replace the note with a count.
///
/// The count is kept because "2 findings withheld" is materially more honest
/// than silence — a reader should know evidence exists and was withheld, not
/// infer from an empty section that there was none.
fn redact_check(check: &CheckResult) -> CheckResult {
    CheckResult {
        check_id: check.check_id.clone(),
        kind: check.kind.clone(),
        status: check.status,
        weight: check.weight,
        // The citations themselves: gone, not blanked.
        evidence: Vec::new(),
        note: redact_note(check.note.as_deref(), check.evidence.len()),
        rationale: check.rationale.clone(),
    }
}

/// Rewrite a collector note for publication.
///
/// Notes are collector-authored and can embed paths — "no match in 202 file(s)
/// matching `**/*.rs`" is harmless, but "21 file(s) excluded by [...]" names
/// internal directories. Rather than pattern-match for safety, the note is
/// replaced with a count-only statement: a rule that cannot be defeated by a
/// note format changing later.
///
/// An already-redacted note passes through unchanged, so redacting twice does
/// not silently drop the withheld count.
fn redact_note(note: Option<&str>, evidence_count: usize) -> Option<String> {
    if let Some(existing) = note {
        if existing.contains(WITHHELD) {
            return Some(existing.to_string());
        }
    }
    match evidence_count {
        0 => None,
        n => Some(format!("{n} {WITHHELD}")),
    }
}

fn redact_control(control: &ControlResult) -> ControlResult {
    ControlResult {
        id: control.id.clone(),
        title: control.title.clone(),
        clause: control.clause.clone(),
        intent: control.intent.clone(),
        severity: control.severity,
        status: control.status,
        checks: control.checks.iter().map(redact_check).collect(),
        rationale: control.rationale.clone(),
        remediation: control.remediation.clone(),
    }
}

impl EvidencePack {
    /// A copy of this pack safe to publish.
    ///
    /// Removes every `file:line` citation and excerpt. Statuses, scores,
    /// rationales and scope notes are preserved, so the report still shows what
    /// was assessed and what the verdict was — just not where to look.
    ///
    /// The workspace path is replaced too: an absolute path leaks a username and
    /// directory layout, and is meaningless to a reader anyway.
    pub fn redacted(&self) -> EvidencePack {
        EvidencePack {
            schema_version: self.schema_version,
            framework: self.framework.clone(),
            workspace: "(redacted)".to_string(),
            generated_at: self.generated_at.clone(),
            scope_note: self.scope_note.clone(),
            controls: self.controls.iter().map(redact_control).collect(),
            score: self.score,
            disabled_capabilities: self.disabled_capabilities.clone(),
        }
    }

    /// Does this pack contain any citation?
    ///
    /// The test hook for "did redaction actually work", used by the export path
    /// as a last-line assertion rather than trusting that it was called.
    pub fn has_citations(&self) -> bool {
        self.controls
            .iter()
            .any(|c| c.checks.iter().any(|k| !k.evidence.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Evidence, FrameworkMeta};
    use crate::status::{ControlStatus, Severity};

    fn pack_with_evidence() -> EvidencePack {
        let evidence = vec![
            Evidence::new(
                "deploy/id_rsa",
                Some(2),
                "-----BEGIN RSA PRIVATE KEY-----",
                "CC6.1/keys",
                "regex",
            ),
            Evidence::new(
                "src/lib.rs",
                Some(40),
                "api_key = \"sk-live-xyz\"",
                "CC6.1/keys",
                "regex",
            )
            .untracked(true),
        ];
        EvidencePack::new(
            FrameworkMeta {
                id: "soc2".into(),
                name: "SOC 2".into(),
                version: "1".into(),
                authority: "AICPA".into(),
            },
            "C:/Users/somebody/private-project".into(),
            "2026-07-25T00:00:00Z".into(),
            "scope note".into(),
            vec![ControlResult {
                id: "CC6.1".into(),
                title: "Logical access".into(),
                clause: "TSC CC6.1".into(),
                intent: "Credentials must not be committed.".into(),
                severity: Severity::Critical,
                status: ControlStatus::Gap,
                checks: vec![CheckResult {
                    check_id: "CC6.1/keys".into(),
                    kind: "regex-must-not-match".into(),
                    status: ControlStatus::Gap,
                    weight: 1.0,
                    evidence,
                    note: Some("21 file(s) excluded by [\"crates/internal/**/*\"]".into()),
                    rationale: "A committed key is a direct failure.".into(),
                }],
                rationale: "all-of: worst of 1 check(s) is gap".into(),
                remediation: Some("Rotate the credential.".into()),
            }],
            vec!["command-exit-code".into()],
        )
    }

    #[test]
    fn redaction_removes_every_citation() {
        let pack = pack_with_evidence();
        assert!(pack.has_citations(), "fixture must start with citations");

        let pub_pack = pack.redacted();
        assert!(!pub_pack.has_citations(), "redacted pack must carry none");
    }

    #[test]
    fn no_file_path_or_excerpt_survives_serialization() {
        // The load-bearing test: serialize the redacted pack and grep the whole
        // document. If a path can reach JSON it can reach HTML.
        let pub_pack = pack_with_evidence().redacted();
        let json = serde_json::to_string(&pub_pack).expect("serialize");

        for leak in [
            "deploy/id_rsa",
            "src/lib.rs",
            "BEGIN RSA PRIVATE KEY",
            "sk-live-xyz",
            "crates/internal",
            "somebody",
            "private-project",
        ] {
            assert!(
                !json.contains(leak),
                "redacted pack leaked {leak:?}:\n{json}"
            );
        }
    }

    #[test]
    fn the_workspace_path_is_replaced() {
        // An absolute path leaks a username and directory layout, and means
        // nothing to a reader.
        let pub_pack = pack_with_evidence().redacted();
        assert_eq!(pub_pack.workspace, "(redacted)");
    }

    #[test]
    fn statuses_and_scores_are_preserved() {
        // Redaction must not change the verdict — a published report that
        // disagreed with the internal one would be worse than useless.
        let pack = pack_with_evidence();
        let pub_pack = pack.redacted();

        assert_eq!(pub_pack.score, pack.score);
        assert_eq!(pub_pack.controls.len(), pack.controls.len());
        assert_eq!(pub_pack.controls[0].status, ControlStatus::Gap);
        assert_eq!(pub_pack.controls[0].severity, Severity::Critical);
        assert_eq!(pub_pack.controls[0].rationale, pack.controls[0].rationale);
    }

    #[test]
    fn intents_rationales_and_remediation_survive() {
        // The judgment is the point of publishing. Keep it.
        let pub_pack = pack_with_evidence().redacted();
        let c = &pub_pack.controls[0];
        assert!(c.intent.contains("Credentials must not be committed"));
        assert!(c.checks[0].rationale.contains("committed key"));
        assert_eq!(c.remediation.as_deref(), Some("Rotate the credential."));
        assert!(!pub_pack.scope_note.is_empty());
    }

    #[test]
    fn withheld_citations_are_counted_not_hidden() {
        // A reader should know evidence exists and was withheld, rather than
        // inferring from silence that there was none.
        let pub_pack = pack_with_evidence().redacted();
        let note = pub_pack.controls[0].checks[0]
            .note
            .as_deref()
            .expect("a withheld count");
        assert!(note.contains('2'), "{note}");
        assert!(note.contains("withheld"), "{note}");
    }

    #[test]
    fn a_check_with_no_evidence_gets_no_spurious_note() {
        let mut pack = pack_with_evidence();
        pack.controls[0].checks[0].evidence.clear();
        pack.controls[0].checks[0].note = Some("no match in 40 file(s)".into());

        let pub_pack = pack.redacted();
        assert_eq!(pub_pack.controls[0].checks[0].note, None);
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = pack_with_evidence().redacted();
        let twice = once.redacted();
        assert_eq!(once, twice);
    }

    #[test]
    fn disabled_capabilities_survive() {
        // A reader must still know commands were not run.
        let pub_pack = pack_with_evidence().redacted();
        assert_eq!(pub_pack.disabled_capabilities, vec!["command-exit-code"]);
    }
}
