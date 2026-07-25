//! The eval suite: real framework controls with hand-labelled expectations.
//!
//! Loading mirrors `sc_eval::TaskSuite` — the house precedent for a TOML-defined
//! suite — but the labels are the interesting part. Each control records what an
//! *honest* draft looks like, which is what makes deterministic grading possible
//! without a judge model.

use std::path::Path;

use sc_proto::{DcError, Result};
use serde::Deserialize;

use crate::draft::DraftRequest;

/// One control to draft, plus what an honest answer looks like.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalControl {
    pub id: String,
    pub framework: String,
    pub title: String,
    #[serde(default)]
    pub clause: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub intent: String,
    /// The control text handed to the model.
    pub text: String,

    /// No repository can evidence this control, so the only honest draft maps
    /// every outcome to `unknown`.
    ///
    /// This is the label that matters. A model returning a check that can
    /// `pass` here has confused a document's existence with a process operating.
    #[serde(default)]
    pub must_be_undeterminable: bool,

    /// The evidence lives in a provider API, so any check reading a
    /// conventionally-absent path must set `on_no_files = "unknown"`.
    #[serde(default)]
    pub expect_provider_side_care: bool,

    /// Genuinely evidenceable from source: a good draft produces checks that can
    /// actually pass. Guards against a model that answers "unknown" to
    /// everything — perfectly honest and perfectly useless.
    #[serde(default)]
    pub expect_real_checks: bool,

    /// Why the label is what it is. For the report, so a reader can disagree.
    #[serde(default)]
    pub note: String,
}

impl EvalControl {
    /// The drafting request for this control.
    pub fn to_request(&self) -> DraftRequest {
        DraftRequest {
            framework: self.framework.clone(),
            control_id: self.id.clone(),
            control_title: self.title.clone(),
            clause: self.clause.clone(),
            intent: self.intent.trim().to_string(),
            severity: if self.severity.is_empty() {
                "medium".to_string()
            } else {
                self.severity.clone()
            },
            control_text: self.text.trim().to_string(),
        }
    }

    /// A short label for the report's category column.
    pub fn category(&self) -> &'static str {
        if self.must_be_undeterminable {
            "organizational"
        } else if self.expect_provider_side_care {
            "provider-side"
        } else {
            "technical"
        }
    }
}

/// The loaded suite.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalSuite {
    pub controls: Vec<EvalControl>,
}

impl EvalSuite {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let suite: EvalSuite =
            toml::from_str(s).map_err(|e| DcError::Comply(format!("parsing eval suite: {e}")))?;
        suite.validate()?;
        Ok(suite)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DcError::Comply(format!("reading eval suite {}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// Reject a suite whose labels contradict each other.
    fn validate(&self) -> Result<()> {
        if self.controls.is_empty() {
            return Err(DcError::Comply("eval suite has no controls".to_string()));
        }
        for c in &self.controls {
            if c.must_be_undeterminable && c.expect_real_checks {
                return Err(DcError::Comply(format!(
                    "control {:?}: cannot both require all-unknown and expect real checks",
                    c.id
                )));
            }
            if c.text.trim().is_empty() {
                return Err(DcError::Comply(format!(
                    "control {:?} has no text to draft from",
                    c.id
                )));
            }
        }
        Ok(())
    }

    /// How many controls are of each category, for the report header.
    pub fn category_counts(&self) -> (usize, usize, usize) {
        let org = self
            .controls
            .iter()
            .filter(|c| c.must_be_undeterminable)
            .count();
        let prov = self
            .controls
            .iter()
            .filter(|c| c.expect_provider_side_care)
            .count();
        (org, prov, self.controls.len() - org - prov)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUITE: &str = include_str!("../../evals/controls.toml");

    #[test]
    fn the_shipped_suite_loads() {
        let suite = EvalSuite::from_toml_str(SUITE).expect("shipped suite must load");
        assert!(
            suite.controls.len() >= 10,
            "only {} controls",
            suite.controls.len()
        );
    }

    #[test]
    fn the_suite_is_weighted_toward_the_traps() {
        // An eval made only of easy technical controls would not measure the
        // property we care about.
        let suite = EvalSuite::from_toml_str(SUITE).expect("loads");
        let (org, prov, tech) = suite.category_counts();
        assert!(org >= 4, "only {org} organizational controls");
        assert!(prov >= 1, "no provider-side controls");
        assert!(tech >= 4, "only {tech} technical controls");
    }

    #[test]
    fn every_control_carries_a_note_justifying_its_label() {
        // The labels encode judgment; a reader must be able to disagree with it.
        let suite = EvalSuite::from_toml_str(SUITE).expect("loads");
        for c in &suite.controls {
            assert!(!c.note.trim().is_empty(), "control {} has no note", c.id);
        }
    }

    #[test]
    fn rejects_contradictory_labels() {
        let src = r#"
[[controls]]
id = "X"
framework = "F"
title = "t"
text = "some text"
must_be_undeterminable = true
expect_real_checks = true
"#;
        let err = EvalSuite::from_toml_str(src).unwrap_err();
        assert!(format!("{err}").contains("cannot both"), "{err}");
    }

    #[test]
    fn rejects_an_empty_suite() {
        assert!(EvalSuite::from_toml_str("controls = []").is_err());
    }

    #[test]
    fn to_request_carries_the_control_through() {
        let suite = EvalSuite::from_toml_str(SUITE).expect("loads");
        let c = suite
            .controls
            .iter()
            .find(|c| c.id == "A.5.1")
            .expect("A.5.1");
        let req = c.to_request();
        assert_eq!(req.control_id, "A.5.1");
        assert!(req.control_text.contains("approved by management"));
        assert_eq!(c.category(), "organizational");
    }
}
