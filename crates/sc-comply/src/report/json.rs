//! JSON rendering. One line of real code, because the types were designed
//! serde-first and round-trip by construction.

use sc_proto::{DcError, Result};

use crate::evidence::EvidencePack;

/// Pretty-printed JSON. Round-trips back into an [`EvidencePack`], which is
/// what lets an auditor diff this quarter's pack against last quarter's.
pub fn render(pack: &EvidencePack) -> Result<String> {
    serde_json::to_string_pretty(pack)
        .map_err(|e| DcError::Comply(format!("serializing evidence pack: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{ControlResult, FrameworkMeta};
    use crate::status::{ControlStatus, Severity};

    fn sample() -> EvidencePack {
        EvidencePack::new(
            FrameworkMeta {
                id: "soc2".into(),
                name: "SOC 2".into(),
                version: "1.0.0".into(),
                authority: "AICPA".into(),
            },
            "/ws".into(),
            "2026-01-01T00:00:00Z".into(),
            "scope".into(),
            vec![ControlResult {
                id: "CC6.1".into(),
                title: "t".into(),
                clause: "c".into(),
                intent: "i".into(),
                severity: Severity::Critical,
                status: ControlStatus::Gap,
                checks: vec![],
                rationale: "r".into(),
                remediation: None,
            }],
            vec!["command-exit-code".into()],
        )
    }

    #[test]
    fn round_trips() {
        let pack = sample();
        let json = render(&pack).expect("render");
        let back: EvidencePack = serde_json::from_str(&json).expect("parse back");
        assert_eq!(pack, back);
    }

    #[test]
    fn carries_a_schema_version_for_downstream_migration() {
        let json = render(&sample()).expect("render");
        assert!(json.contains("\"schema_version\": 1"), "{json}");
    }

    #[test]
    fn statuses_serialize_as_kebab_case() {
        let mut pack = sample();
        pack.controls[0].status = ControlStatus::NotApplicable;
        let json = render(&pack).expect("render");
        assert!(json.contains("\"not-applicable\""), "{json}");
    }
}
