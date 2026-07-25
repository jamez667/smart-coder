//! Key-path assertions over TOML and JSON configuration files.
//!
//! Deliberately no YAML in v1. It would need a new dependency (`serde_yaml` is
//! unmaintained), and YAML's coercion rules — `yes` versus `true`, anchors,
//! multi-document streams — make key-path assertions ambiguous in ways that
//! matter when the output is an audit artifact. CI YAML is served adequately by
//! `regex-match-in-glob` plus an honest `on_no_files`.

use regex::Regex;
use sc_proto::{DcError, Result};

use crate::collector::{AuditContext, Collector, Observation};
use crate::evidence::Evidence;
use crate::pack::{Assertion, Check, CheckKind};

/// Handles `toml-path` and `json-path`.
pub struct StructuredCollector;

/// A value extracted from either format, normalized so assertions are written
/// once rather than per-format.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
    Array(usize),
    Table(usize),
    Null,
}

impl Value {
    fn render(&self) -> String {
        match self {
            Value::Str(s) => format!("{s:?}"),
            Value::Num(n) => format!("{n}"),
            Value::Bool(b) => format!("{b}"),
            Value::Array(n) => format!("<array of {n}>"),
            Value::Table(n) => format!("<table of {n}>"),
            Value::Null => "null".to_string(),
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            // A numeric string is still a number for threshold purposes; config
            // formats quote numbers inconsistently.
            Value::Str(s) => s.parse().ok(),
            Value::Bool(_) | Value::Array(_) | Value::Table(_) | Value::Null => None,
        }
    }

    fn is_non_empty(&self) -> bool {
        match self {
            Value::Str(s) => !s.is_empty(),
            Value::Array(n) | Value::Table(n) => *n > 0,
            Value::Num(_) | Value::Bool(_) => true,
            Value::Null => false,
        }
    }

    fn text(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Num(n) => format!("{n}"),
            Value::Bool(b) => format!("{b}"),
            other => other.render(),
        }
    }
}

impl Collector for StructuredCollector {
    fn name(&self) -> &'static str {
        "structured"
    }

    fn handles(&self, kind: &CheckKind) -> bool {
        matches!(
            kind,
            CheckKind::TomlPath { .. } | CheckKind::JsonPath { .. }
        )
    }

    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation> {
        let (path, key_path, assertion, is_toml) = match &check.kind {
            CheckKind::TomlPath {
                path,
                key_path,
                assert,
            } => (path, key_path, assert, true),
            CheckKind::JsonPath {
                path,
                key_path,
                assert,
            } => (path, key_path, assert, false),
            other => {
                return Err(DcError::Comply(format!(
                    "StructuredCollector cannot handle {}",
                    other.label()
                )))
            }
        };

        let Some(file) = ctx.files.iter().find(|f| f.path == *path) else {
            // The canonical case: `.github/settings.yml` is usually absent
            // because branch protection lives in the provider's API. That is
            // not evidence the control fails.
            return Ok(Observation::indeterminate(format!(
                "{path} not found in the workspace"
            )));
        };

        let value = if is_toml {
            match toml::from_str::<toml::Value>(&file.contents) {
                Ok(root) => lookup_toml(&root, key_path),
                Err(e) => {
                    return Ok(Observation::indeterminate(format!(
                        "{path} is not parseable as TOML: {e}"
                    )))
                }
            }
        } else {
            match serde_json::from_str::<serde_json::Value>(&file.contents) {
                Ok(root) => lookup_json(&root, key_path),
                Err(e) => {
                    return Ok(Observation::indeterminate(format!(
                        "{path} is not parseable as JSON: {e}"
                    )))
                }
            }
        };

        let Some(value) = value else {
            // The file parsed but the key is absent. That IS a definite
            // negative — we looked at a well-formed document and the setting
            // was not there.
            return Ok(Observation {
                matched: Some(false),
                evidence: vec![Evidence::new(
                    path,
                    None,
                    format!("{key_path} is not set"),
                    &check.id,
                    self.name(),
                )],
                note: Some(format!("{path}: key path {key_path:?} not present")),
            });
        };

        let holds = evaluate(assertion, &value, &check.id)?;
        let evidence = vec![Evidence::new(
            path,
            None,
            format!("{key_path} = {}", value.render()),
            &check.id,
            self.name(),
        )];

        Ok(Observation {
            matched: Some(holds),
            evidence,
            note: None,
        })
    }
}

fn evaluate(assertion: &Assertion, value: &Value, check_id: &str) -> Result<bool> {
    Ok(match assertion {
        Assertion::Exists => true,
        Assertion::NonEmpty => value.is_non_empty(),
        Assertion::Equals { value: want } => toml_value_matches(want, value),
        Assertion::NotEquals { value: want } => !toml_value_matches(want, value),
        Assertion::Gte { value: want } => value.as_number().map(|n| n >= *want).unwrap_or(false),
        Assertion::Lte { value: want } => value.as_number().map(|n| n <= *want).unwrap_or(false),
        Assertion::Matches { pattern } => {
            let re = Regex::new(pattern).map_err(|e| {
                DcError::Comply(format!("check {check_id:?}: invalid assert regex: {e}"))
            })?;
            re.is_match(&value.text())
        }
    })
}

/// Compare a pack-declared TOML literal against an extracted value.
fn toml_value_matches(want: &toml::Value, got: &Value) -> bool {
    match (want, got) {
        (toml::Value::String(a), Value::Str(b)) => a == b,
        (toml::Value::Integer(a), Value::Num(b)) => (*a as f64 - b).abs() < f64::EPSILON,
        (toml::Value::Float(a), Value::Num(b)) => (a - b).abs() < f64::EPSILON,
        (toml::Value::Boolean(a), Value::Bool(b)) => a == b,
        // Cross-type comparison against the rendered text, so a pack can write
        // `value = "1"` against a JSON number without surprise.
        (toml::Value::String(a), other) => a == &other.text(),
        _ => false,
    }
}

/// Walk a dotted key path through a TOML document. Numeric segments index
/// arrays.
fn lookup_toml(root: &toml::Value, key_path: &str) -> Option<Value> {
    let mut cur = root;
    for seg in key_path.split('.') {
        cur = match cur {
            toml::Value::Table(t) => t.get(seg)?,
            toml::Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(match cur {
        toml::Value::String(s) => Value::Str(s.clone()),
        toml::Value::Integer(i) => Value::Num(*i as f64),
        toml::Value::Float(f) => Value::Num(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Array(a) => Value::Array(a.len()),
        toml::Value::Table(t) => Value::Table(t.len()),
        toml::Value::Datetime(d) => Value::Str(d.to_string()),
    })
}

/// Walk a dotted key path through a JSON document. Numeric segments index
/// arrays, so `branches.0.protection` works.
fn lookup_json(root: &serde_json::Value, key_path: &str) -> Option<Value> {
    let mut cur = root;
    for seg in key_path.split('.') {
        cur = match cur {
            serde_json::Value::Object(m) => m.get(seg)?,
            serde_json::Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(match cur {
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Number(n) => Value::Num(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Array(a) => Value::Array(a.len()),
        serde_json::Value::Object(m) => Value::Table(m.len()),
        serde_json::Value::Null => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::ComplyOptions;
    use crate::scan::TextFile;
    use crate::status::Outcome;
    use std::path::Path;

    fn file(path: &str, contents: &str) -> TextFile {
        TextFile {
            path: path.to_string(),
            contents: contents.to_string(),
            ignored: false,
        }
    }

    fn json_check(id: &str, path: &str, key_path: &str, assert: Assertion) -> Check {
        Check {
            id: id.to_string(),
            kind: CheckKind::JsonPath {
                path: path.to_string(),
                key_path: key_path.to_string(),
                assert,
            },
            on_match: Outcome::Pass,
            on_no_match: Outcome::Gap,
            on_no_files: Some(Outcome::Unknown),
            weight: 1.0,
            exclude_globs: vec![],
            rationale: String::new(),
        }
    }

    fn toml_check(id: &str, path: &str, key_path: &str, assert: Assertion) -> Check {
        Check {
            id: id.to_string(),
            kind: CheckKind::TomlPath {
                path: path.to_string(),
                key_path: key_path.to_string(),
                assert,
            },
            on_match: Outcome::Pass,
            on_no_match: Outcome::Gap,
            on_no_files: Some(Outcome::Unknown),
            weight: 1.0,
            exclude_globs: vec![],
            rationale: String::new(),
        }
    }

    fn run(files: &[TextFile], c: &Check) -> Observation {
        let opts = ComplyOptions::default();
        let ctx = AuditContext::new(Path::new("/ws"), files, &opts);
        StructuredCollector.collect(c, &ctx).expect("collect")
    }

    const SETTINGS: &str = r#"{
      "branches": [
        { "name": "main",
          "protection": {
            "required_pull_request_reviews": { "required_approving_review_count": 2 }
          }
        }
      ]
    }"#;

    #[test]
    fn json_path_indexes_arrays_and_evaluates_gte() {
        let files = vec![file(".github/settings.json", SETTINGS)];
        let c = json_check(
            "review",
            ".github/settings.json",
            "branches.0.protection.required_pull_request_reviews.required_approving_review_count",
            Assertion::Gte { value: 1.0 },
        );
        let o = run(&files, &c);
        assert_eq!(o.matched, Some(true));
        assert!(
            o.evidence[0].excerpt.contains("= 2"),
            "{:?}",
            o.evidence[0].excerpt
        );
    }

    #[test]
    fn gte_fails_below_the_threshold() {
        let files = vec![file(".github/settings.json", SETTINGS)];
        let c = json_check(
            "review",
            ".github/settings.json",
            "branches.0.protection.required_pull_request_reviews.required_approving_review_count",
            Assertion::Gte { value: 3.0 },
        );
        assert_eq!(run(&files, &c).matched, Some(false));
    }

    #[test]
    fn a_missing_file_is_indeterminate_not_a_gap() {
        // Branch protection normally lives in the provider's API. Its absence
        // from the repo is not evidence that review is unenforced.
        let files = vec![file("other.json", "{}")];
        let c = json_check(
            "review",
            ".github/settings.json",
            "branches.0",
            Assertion::Exists,
        );
        let o = run(&files, &c);
        assert_eq!(o.matched, None);
        assert!(o.note.expect("note").contains("not found"));
    }

    #[test]
    fn a_present_file_with_a_missing_key_is_a_definite_negative() {
        // We looked at a well-formed document and the setting was absent —
        // genuinely different from not being able to look at all.
        let files = vec![file(".github/settings.json", "{\"branches\": []}")];
        let c = json_check(
            "review",
            ".github/settings.json",
            "branches.0.protection",
            Assertion::Exists,
        );
        let o = run(&files, &c);
        assert_eq!(o.matched, Some(false));
        assert!(
            !o.evidence.is_empty(),
            "a negative still cites the file it read"
        );
    }

    #[test]
    fn an_unparseable_file_is_indeterminate() {
        let files = vec![file(".github/settings.json", "{not json")];
        let c = json_check("review", ".github/settings.json", "a", Assertion::Exists);
        let o = run(&files, &c);
        assert_eq!(o.matched, None);
        assert!(o.note.expect("note").contains("not parseable"));
    }

    #[test]
    fn toml_path_walks_tables() {
        let files = vec![file(
            "Cargo.toml",
            "[package]\nname = \"x\"\nedition = \"2021\"\n",
        )];
        let c = toml_check(
            "edition",
            "Cargo.toml",
            "package.edition",
            Assertion::Equals {
                value: toml::Value::String("2021".into()),
            },
        );
        assert_eq!(run(&files, &c).matched, Some(true));
    }

    #[test]
    fn assertion_variants_behave() {
        let files = vec![file(
            "c.toml",
            "[a]\ns = \"hello\"\nn = 5\nb = true\nempty = \"\"\nlist = [1, 2]\n",
        )];
        let cases: Vec<(&str, Assertion, bool)> = vec![
            ("a.s", Assertion::Exists, true),
            ("a.missing", Assertion::Exists, false),
            ("a.s", Assertion::NonEmpty, true),
            ("a.empty", Assertion::NonEmpty, false),
            ("a.list", Assertion::NonEmpty, true),
            ("a.n", Assertion::Gte { value: 5.0 }, true),
            ("a.n", Assertion::Lte { value: 4.0 }, false),
            (
                "a.b",
                Assertion::Equals {
                    value: toml::Value::Boolean(true),
                },
                true,
            ),
            (
                "a.b",
                Assertion::NotEquals {
                    value: toml::Value::Boolean(true),
                },
                false,
            ),
            (
                "a.s",
                Assertion::Matches {
                    pattern: "^hel".into(),
                },
                true,
            ),
            (
                "a.s",
                Assertion::Matches {
                    pattern: "^bye".into(),
                },
                false,
            ),
        ];
        for (key, assertion, want) in cases {
            let c = toml_check("t", "c.toml", key, assertion);
            let o = run(&files, &c);
            // `Exists` on a missing key resolves via the missing-key path.
            let got = o.matched.unwrap_or(false);
            assert_eq!(got, want, "key {key} gave {got}, wanted {want}");
        }
    }

    #[test]
    fn numeric_string_compares_as_a_number() {
        // Config formats quote numbers inconsistently; a threshold check should
        // not be defeated by quoting.
        let files = vec![file("c.json", "{\"count\": \"2\"}")];
        let c = json_check("n", "c.json", "count", Assertion::Gte { value: 1.0 });
        assert_eq!(run(&files, &c).matched, Some(true));
    }

    #[test]
    fn gte_against_a_non_numeric_value_is_false_not_an_error() {
        let files = vec![file("c.json", "{\"count\": \"many\"}")];
        let c = json_check("n", "c.json", "count", Assertion::Gte { value: 1.0 });
        assert_eq!(run(&files, &c).matched, Some(false));
    }

    #[test]
    fn rejects_a_kind_it_does_not_handle() {
        let c = Check {
            id: "x".into(),
            kind: CheckKind::FileAbsent {
                path: ".env".into(),
            },
            on_match: Outcome::Gap,
            on_no_match: Outcome::Pass,
            on_no_files: None,
            weight: 1.0,
            exclude_globs: vec![],
            rationale: String::new(),
        };
        assert!(!StructuredCollector.handles(&c.kind));
        let files: Vec<TextFile> = vec![];
        let opts = ComplyOptions::default();
        let ctx = AuditContext::new(Path::new("/ws"), &files, &opts);
        assert!(StructuredCollector.collect(&c, &ctx).is_err());
    }
}
