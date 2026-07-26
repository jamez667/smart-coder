//! SARIF 2.1.0 output for the code-anchored subset.
//!
//! # Unknowns are deliberately excluded — do not "fix" this
//!
//! Only `Gap` findings with a `file:line` anchor are emitted. A code-scanning
//! UI has no concept of "we could not tell": every SARIF result renders as a
//! finding against the code. Emitting Unknowns as `note`-level results would
//! collapse the very distinction the status lattice exists to preserve, and
//! would put "the auditor must fetch a document from the VCS provider" in front
//! of an engineer as though it were a defect in their file.
//!
//! Unknowns live in the Markdown and JSON outputs, where the distinction
//! survives. If you need them in a code-scanning UI, the right fix is a
//! different output format, not a widened filter here.
//!
//! One rule per *control* rather than per check, so an alert reads "CC6.1" —
//! the language an auditor and an engineer share.

use std::collections::BTreeMap;

use sc_proto::{DcError, Result};
use serde_json::{json, Value};

use crate::evidence::{EvidencePack, Finding};
use crate::status::ControlStatus;

/// Render the code-anchored gaps as a SARIF 2.1.0 log.
pub fn render(pack: &EvidencePack) -> Result<String> {
    let findings: Vec<Finding> = pack
        .findings()
        .into_iter()
        .filter(|f| f.status == ControlStatus::Gap && f.anchor().is_some())
        .collect();

    // One rule per control, deduplicated and ordered for stable output.
    let mut rules: BTreeMap<String, Value> = BTreeMap::new();
    for f in &findings {
        rules.entry(f.control_id.clone()).or_insert_with(|| {
            json!({
                "id": f.control_id,
                "name": f.control_title,
                "shortDescription": { "text": f.control_title },
                "fullDescription": { "text": f.clause },
                "defaultConfiguration": { "level": f.severity.sarif_level() },
                "properties": {
                    "clause": f.clause,
                    "severity": f.severity.label(),
                },
            })
        });
    }

    let results: Vec<Value> = findings
        .iter()
        .filter_map(|f| {
            let (file, line) = f.anchor()?;
            Some(json!({
                "ruleId": f.control_id,
                "level": f.severity.sarif_level(),
                "message": { "text": f.summary },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": file },
                        "region": { "startLine": line },
                    }
                }],
            }))
        })
        .collect();

    let log = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "sc-comply",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://github.com/jamez667/smart-coder",
                    "rules": rules.into_values().collect::<Vec<_>>(),
                }
            },
            "results": results,
        }],
    });

    serde_json::to_string_pretty(&log)
        .map_err(|e| DcError::Comply(format!("serializing SARIF: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{CheckResult, ControlResult, Evidence, FrameworkMeta};
    use crate::status::Severity;

    fn control(
        id: &str,
        status: ControlStatus,
        severity: Severity,
        evidence: Vec<Evidence>,
    ) -> ControlResult {
        ControlResult {
            id: id.to_string(),
            title: format!("{id} title"),
            section: Default::default(),
            clause: format!("TSC {id}"),
            intent: "i".into(),
            severity,
            status,
            checks: vec![CheckResult {
                check_id: format!("{id}/c"),
                kind: "regex-must-not-match".into(),
                status,
                weight: 1.0,
                evidence,
                note: None,
                rationale: "r".into(),
            }],
            rationale: "determined".into(),
            remediation: None,
        }
    }

    fn pack_with(controls: Vec<ControlResult>) -> EvidencePack {
        EvidencePack::new(
            FrameworkMeta {
                id: "soc2".into(),
                name: "SOC 2".into(),
                version: "1".into(),
                authority: "AICPA".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            "scope".into(),
            controls,
            vec![],
        )
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("SARIF output must be valid JSON")
    }

    fn results(v: &Value) -> &Vec<Value> {
        v["runs"][0]["results"].as_array().expect("results array")
    }

    #[test]
    fn emits_a_result_for_an_anchored_gap() {
        let pack = pack_with(vec![control(
            "CC6.1",
            ControlStatus::Gap,
            Severity::Critical,
            vec![Evidence::new(
                "deploy/id_rsa",
                Some(2),
                "KEY",
                "CC6.1/c",
                "regex",
            )],
        )]);
        let v = parse(&render(&pack).expect("render"));

        assert_eq!(results(&v).len(), 1);
        let r = &results(&v)[0];
        assert_eq!(r["ruleId"], "CC6.1");
        assert_eq!(r["level"], "error");
        assert_eq!(
            r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "deploy/id_rsa"
        );
        assert_eq!(
            r["locations"][0]["physicalLocation"]["region"]["startLine"],
            2
        );
    }

    #[test]
    fn unknowns_produce_no_results() {
        // The load-bearing exclusion. A code-scanning UI cannot render
        // "we could not tell", so unknowns must never reach it.
        let pack = pack_with(vec![control(
            "CC8.1",
            ControlStatus::Unknown,
            Severity::High,
            vec![Evidence::new("x.yml", Some(3), "?", "CC8.1/c", "regex")],
        )]);
        let v = parse(&render(&pack).expect("render"));
        assert!(
            results(&v).is_empty(),
            "an Unknown must not become a SARIF result"
        );
    }

    #[test]
    fn errors_produce_no_results() {
        let pack = pack_with(vec![control(
            "CC7.2",
            ControlStatus::Error,
            Severity::Medium,
            vec![Evidence::new("x.rs", Some(1), "?", "CC7.2/c", "regex")],
        )]);
        let v = parse(&render(&pack).expect("render"));
        assert!(results(&v).is_empty(), "a tool error is not a code finding");
    }

    #[test]
    fn gaps_without_a_code_anchor_are_omitted() {
        // Organizational gaps have no file:line and would render as a finding
        // against an arbitrary file.
        let pack = pack_with(vec![control(
            "CC2.3",
            ControlStatus::Gap,
            Severity::Medium,
            vec![Evidence::new(
                "SECURITY.md",
                None,
                "absent",
                "CC2.3/c",
                "file",
            )],
        )]);
        let v = parse(&render(&pack).expect("render"));
        assert!(results(&v).is_empty());
    }

    #[test]
    fn one_rule_per_control_even_with_several_findings() {
        let pack = pack_with(vec![control(
            "CC6.1",
            ControlStatus::Gap,
            Severity::Critical,
            vec![
                Evidence::new("a.rs", Some(1), "KEY", "CC6.1/c", "regex"),
                Evidence::new("b.rs", Some(9), "KEY", "CC6.1/c", "regex"),
            ],
        )]);
        let v = parse(&render(&pack).expect("render"));
        let rules = v["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .expect("rules");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["id"], "CC6.1");
        // The finding anchors to the first line-bearing citation.
        assert_eq!(results(&v).len(), 1);
    }

    #[test]
    fn severity_maps_to_sarif_levels() {
        for (sev, want) in [
            (Severity::Critical, "error"),
            (Severity::High, "error"),
            (Severity::Medium, "warning"),
            (Severity::Low, "note"),
        ] {
            let pack = pack_with(vec![control(
                "CC1.1",
                ControlStatus::Gap,
                sev,
                vec![Evidence::new("a.rs", Some(1), "x", "CC1.1/c", "regex")],
            )]);
            let v = parse(&render(&pack).expect("render"));
            assert_eq!(results(&v)[0]["level"], want, "severity {sev:?}");
        }
    }

    #[test]
    fn output_is_well_formed_sarif_even_when_empty() {
        let v = parse(&render(&pack_with(vec![])).expect("render"));
        assert_eq!(v["version"], "2.1.0");
        assert_eq!(v["runs"][0]["tool"]["driver"]["name"], "sc-comply");
        assert!(results(&v).is_empty());
    }
}
