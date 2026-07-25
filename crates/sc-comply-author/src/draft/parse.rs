//! Parsing a model's drafted checks.
//!
//! Follows the house pattern: [`extract_json_array`] does a balanced-bracket
//! scan that tolerates markdown fences and surrounding prose *by construction*,
//! so no fence-stripping is needed.
//!
//! Where this departs from `sc-core`'s planner convention: a parse failure is
//! **reported**, not silently degraded to an empty result. The planner can fall
//! back to a generic step and keep the agent moving; a drafting tool that
//! quietly produces nothing has simply wasted the author's time and tokens.
//! Errors here are also the retry feedback, so they must name the exact problem.

use sc_comply::pack::CheckKind;
use sc_comply::status::Outcome;
use serde::Deserialize;

/// One check as the model proposes it, before it becomes TOML.
///
/// `kind` deserializes into sc-comply's own closed [`CheckKind`] enum, so a
/// model that invents `"grep-file"` fails here with a usable error rather than
/// producing plausible TOML that breaks at audit time.
#[derive(Debug, Clone, Deserialize)]
pub struct DraftCheck {
    pub id: String,
    #[serde(flatten)]
    pub kind: CheckKind,
    pub on_match: Outcome,
    pub on_no_match: Outcome,
    #[serde(default)]
    pub on_no_files: Option<Outcome>,
    #[serde(default)]
    pub weight: Option<f64>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    #[serde(default)]
    pub rationale: String,
}

/// The outcome of parsing one model reply.
#[derive(Debug, Clone, Default)]
pub struct ParseOutcome {
    pub checks: Vec<DraftCheck>,
    /// Per-item problems, phrased for a model to act on.
    pub errors: Vec<String>,
}

impl ParseOutcome {
    pub fn is_usable(&self) -> bool {
        !self.checks.is_empty() && self.errors.is_empty()
    }
}

/// Parse a model reply into drafted checks.
///
/// Partial success is preserved: valid checks are kept and invalid ones become
/// errors, so a reply that is four-fifths right can be repaired by a retry that
/// only has to fix the fifth.
pub fn parse_drafts(reply: &str) -> ParseOutcome {
    let Some(arr) = sc_core::extract_json_array(reply) else {
        return ParseOutcome {
            checks: vec![],
            errors: vec![
                "no JSON array found in the reply; return ONLY a JSON array of check objects"
                    .to_string(),
            ],
        };
    };

    let items: Vec<serde_json::Value> = match serde_json::from_str(arr) {
        Ok(v) => v,
        Err(e) => {
            return ParseOutcome {
                checks: vec![],
                errors: vec![format!("the JSON array is malformed: {e}")],
            }
        }
    };

    if items.is_empty() {
        return ParseOutcome {
            checks: vec![],
            errors: vec![
                "the array was empty; propose at least one check, or an all-unknown check if \
                 the control cannot be evidenced from source"
                    .to_string(),
            ],
        };
    }

    let mut out = ParseOutcome::default();
    for (i, item) in items.iter().enumerate() {
        match serde_json::from_value::<DraftCheck>(item.clone()) {
            Ok(c) => out.checks.push(c),
            Err(e) => out.errors.push(describe_item_error(i, item, &e)),
        }
    }
    out
}

/// Turn a serde error into something a model can act on.
///
/// The default message for a failed internally-tagged enum is unhelpfully
/// generic, and an unknown `kind` is by far the most common failure — so it gets
/// named explicitly along with the closed vocabulary.
fn describe_item_error(index: usize, item: &serde_json::Value, err: &serde_json::Error) -> String {
    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("<no id>");

    if let Some(kind) = item.get("kind").and_then(|v| v.as_str()) {
        const KNOWN: &[&str] = &[
            "file-exists",
            "file-absent",
            "regex-match-in-glob",
            "regex-must-not-match",
            "symbol-exists",
            "toml-path",
            "json-path",
            "command-exit-code",
        ];
        if !KNOWN.contains(&kind) {
            return format!(
                "check[{index}] {id:?}: `{kind}` is not a valid check kind. Use exactly one of: {}",
                KNOWN.join(", ")
            );
        }
        return format!(
            "check[{index}] {id:?} (kind `{kind}`): {err}. Check the required fields for that kind."
        );
    }
    format!("check[{index}] {id:?}: missing a `kind` field ({err})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"[
      {"id":"ci-runs-tests","kind":"regex-match-in-glob",
       "glob":".github/workflows/*.yml","pattern":"cargo test",
       "on_match":"pass","on_no_match":"gap","on_no_files":"gap",
       "rationale":"The testing gate is evidenced in the pipeline definition."}
    ]"#;

    #[test]
    fn parses_a_clean_array() {
        let out = parse_drafts(GOOD);
        assert!(out.is_usable(), "{:?}", out.errors);
        assert_eq!(out.checks.len(), 1);
        assert_eq!(out.checks[0].id, "ci-runs-tests");
        assert_eq!(out.checks[0].on_no_files, Some(Outcome::Gap));
    }

    #[test]
    fn tolerates_markdown_fences_and_prose() {
        // The house behaviour: extract_json_array skips the fence by construction.
        let reply = format!("Here are the checks:\n\n```json\n{GOOD}\n```\n\nHope that helps!");
        let out = parse_drafts(&reply);
        assert!(out.is_usable(), "{:?}", out.errors);
        assert_eq!(out.checks.len(), 1);
    }

    #[test]
    fn an_invented_kind_produces_a_usable_error() {
        // The most common model failure, and the retry depends on this text.
        let out = parse_drafts(
            r#"[{"id":"x","kind":"grep-file","on_match":"pass","on_no_match":"gap"}]"#,
        );
        assert!(out.checks.is_empty());
        assert_eq!(out.errors.len(), 1);
        let e = &out.errors[0];
        assert!(e.contains("grep-file"), "{e}");
        assert!(e.contains("not a valid check kind"), "{e}");
        assert!(
            e.contains("regex-match-in-glob"),
            "must list the vocabulary: {e}"
        );
    }

    #[test]
    fn a_known_kind_with_wrong_fields_names_the_kind() {
        // regex-match-in-glob without `pattern`.
        let out = parse_drafts(
            r#"[{"id":"x","kind":"regex-match-in-glob","glob":"**/*","on_match":"pass","on_no_match":"gap"}]"#,
        );
        assert!(out.checks.is_empty());
        assert!(
            out.errors[0].contains("regex-match-in-glob"),
            "{:?}",
            out.errors
        );
    }

    #[test]
    fn partial_success_keeps_the_good_checks() {
        // A retry then only has to fix the broken one.
        let reply = r#"[
          {"id":"ok","kind":"file-exists","paths":["README.md"],
           "on_match":"pass","on_no_match":"gap"},
          {"id":"bad","kind":"telepathy","on_match":"pass","on_no_match":"gap"}
        ]"#;
        let out = parse_drafts(reply);
        assert_eq!(out.checks.len(), 1);
        assert_eq!(out.errors.len(), 1);
        assert!(!out.is_usable(), "errors must block acceptance");
    }

    #[test]
    fn no_json_at_all_is_an_error_not_silence() {
        let out = parse_drafts("I'm sorry, I can't help with that.");
        assert!(out.checks.is_empty());
        assert!(out.errors[0].contains("no JSON array"), "{:?}", out.errors);
    }

    #[test]
    fn an_empty_array_asks_for_the_unknown_form() {
        let out = parse_drafts("[]");
        assert!(out.checks.is_empty());
        assert!(out.errors[0].contains("all-unknown"), "{:?}", out.errors);
    }

    #[test]
    fn truncated_json_reports_malformed() {
        let out = parse_drafts(r#"[{"id":"x","kind":"file-exists","paths":["a"]"#);
        assert!(out.checks.is_empty());
        assert!(!out.errors.is_empty());
    }

    #[test]
    fn optional_fields_default_cleanly() {
        let out = parse_drafts(
            r#"[{"id":"x","kind":"file-exists","paths":["a"],"on_match":"pass","on_no_match":"gap"}]"#,
        );
        let c = &out.checks[0];
        assert_eq!(c.on_no_files, None);
        assert_eq!(c.weight, None);
        assert!(c.exclude_globs.is_empty());
        assert!(c.rationale.is_empty());
    }
}
