//! The Phase-4 test plan (spec 09, amended): the orchestrator lists the *coverage*
//! each test file must hit; small worker models then write the actual tests.
//!
//! This keeps the architect doing what it's good at (deciding what behavior to
//! pin) and the cheap workers doing the writing — the same labour split as the
//! implementation swarm (spec 08).

use std::collections::BTreeMap;

use sc_core::extract_json_array;
use serde::{Deserialize, Serialize};

/// One behavior a test must check, scoped to a test file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageItem {
    pub file: String,
    pub covers: String,
    /// The exact JSON body the route must return for this behavior, as a literal
    /// string (e.g. `{"name": "x", "value": 2}`), when the orchestrator specifies one.
    /// The test-writer asserts it verbatim, and a code check (in `write_tests`) rejects
    /// a generated test that drops any of its keys — the 8B systematically omits fields
    /// otherwise (observed 2026-06-15: 0/3 included `name`). `None` for non-JSON behaviors.
    #[serde(default)]
    pub expect: Option<String>,
}

/// Parse a Phase-4 artifact (a JSON array of `{file, covers}`, tolerating
/// surrounding prose) into coverage items. Empty if nothing parseable.
pub fn parse_coverage(reply: &str) -> Vec<CoverageItem> {
    let Some(arr) = extract_json_array(reply) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arr) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let raw = item
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let covers = item
            .get("covers")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // `expect` may be a JSON literal (object) or a string the orchestrator wrote.
        // Accept either: an object is re-serialized compactly; a string is taken as-is.
        let expect = match item.get("expect") {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
                Some(s.trim().to_string())
            }
            Some(v @ serde_json::Value::Object(_)) => Some(v.to_string()),
            _ => None,
        };
        if !raw.is_empty() && !covers.is_empty() {
            // Force a valid pytest filename. pytest only discovers `test_*.py`, so a
            // name the model glued from a source file (e.g. `test_index.html.js` for a
            // frontend file) is invisible — the subtask it gates can never pass and gets
            // reverted (observed live 2026-06-14). Normalize to `test_<stem>.py`.
            out.push(CoverageItem {
                file: pytest_name(&raw),
                covers,
                expect,
            });
        }
    }
    out
}

/// Coerce a coverage `file` into a test filename a runner can actually discover. The
/// runner is per-language (spec 08): Python backend tests are `test_*.py` (pytest),
/// frontend tests are `*.test.js` (vitest). A model often glues `test_` onto the source
/// filename including its extension (`test_index.html.js`) — which neither runner finds
/// — so we normalize to the right shape for the target language.
///   `test_app.py` → `test_app.py`              (pytest, unchanged)
///   `script.js` / `test_script.js` → `script.test.js`   (vitest)
///   `test_index.html.js` / `index.html` → `index.test.js` (vitest — a frontend file)
///   `home.css` → `home.test.js`                 (vitest — a frontend asset)
fn pytest_name(raw: &str) -> String {
    let raw = raw.replace('\\', "/");
    let (dir, base) = match raw.rsplit_once('/') {
        Some((d, b)) => (format!("{d}/"), b.to_string()),
        None => (String::new(), raw),
    };
    let lower = base.to_ascii_lowercase();
    // A JS/frontend test: an explicit vitest name, or a name targeting a frontend asset.
    let is_js = lower.ends_with(".test.js")
        || lower.ends_with(".spec.js")
        || lower.contains(".js")
        || lower.contains(".html")
        || lower.contains(".css");
    // Bare stem: drop every extension and a leading `test_`.
    let stem = base.split('.').next().unwrap_or(&base);
    let stem = stem.strip_prefix("test_").unwrap_or(stem);
    let stem = if stem.is_empty() { "module" } else { stem };
    if is_js {
        format!("{dir}{stem}.test.js")
    } else {
        format!("{dir}test_{stem}.py")
    }
}

/// Group coverage items by test file, preserving the order each file first
/// appears. One group → one test-writing worker (file-granularity rule, spec 08).
/// The full items are kept (not just the `covers` prose) so the writer prompt and the
/// field-coverage enforcement can use each behavior's `expect` literal.
pub fn group_by_file(items: &[CoverageItem]) -> Vec<(String, Vec<CoverageItem>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, Vec<CoverageItem>> = BTreeMap::new();
    for it in items {
        if !map.contains_key(&it.file) {
            order.push(it.file.clone());
        }
        map.entry(it.file.clone()).or_default().push(it.clone());
    }
    order
        .into_iter()
        .map(|f| {
            let group = map.remove(&f).unwrap_or_default();
            (f, group)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_coverage_array() {
        let reply = r#"[
            {"file":"test_csv.py","covers":"empty input returns []"},
            {"file":"test_csv.py","covers":"one row parses to one dict"}
        ]"#;
        let items = parse_coverage(reply);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].file, "test_csv.py");
        assert!(items[1].covers.contains("one row"));
    }

    #[test]
    fn coerces_test_filenames_per_language() {
        // The live bug: mangled frontend test names (test_index.html.js) that NO runner
        // finds. Backend → test_*.py (pytest); frontend → *.test.js (vitest).
        let reply = r#"[
            {"file":"test_index.html.js","covers":"home serves Hello World"},
            {"file":"static/style.css","covers":"page is styled"},
            {"file":"script.js","covers":"button toggles"},
            {"file":"test_app.py","covers":"root route 200"}
        ]"#;
        let items = parse_coverage(reply);
        let files: Vec<&str> = items.iter().map(|i| i.file.as_str()).collect();
        // Backend Python test: unchanged, pytest-discoverable.
        assert!(files.contains(&"test_app.py"), "{files:?}");
        // Frontend tests: vitest-discoverable `*.test.js`, never the mangled form.
        assert!(files.contains(&"index.test.js"), "{files:?}");
        assert!(files.contains(&"static/style.test.js"), "{files:?}");
        assert!(files.contains(&"script.test.js"), "{files:?}");
        assert!(
            !files
                .iter()
                .any(|f| f.contains(".html.js") || f.contains(".css.js")),
            "no mangled double-extension names: {files:?}"
        );
    }

    #[test]
    fn tolerates_prose_and_skips_incomplete() {
        let reply =
            "Here is the plan:\n[{\"file\":\"t.py\",\"covers\":\"x\"},{\"file\":\"t.py\"}]\ndone";
        let items = parse_coverage(reply);
        assert_eq!(items.len(), 1); // the item missing `covers` is dropped
    }

    fn item(file: &str, covers: &str) -> CoverageItem {
        CoverageItem {
            file: file.into(),
            covers: covers.into(),
            expect: None,
        }
    }

    #[test]
    fn parses_expect_json_literal() {
        let reply = r#"[
            {"file":"test_app.py","covers":"incr returns name+value","expect":{"name":"x","value":1}},
            {"file":"test_app.py","covers":"missing is 404","expect":"{\"error\":\"not found\"}"}
        ]"#;
        let items = parse_coverage(reply);
        assert_eq!(items.len(), 2);
        // An object literal is serialized; a string literal is taken as-is.
        assert!(items[0].expect.as_deref().unwrap().contains("\"name\""));
        assert!(items[0].expect.as_deref().unwrap().contains("\"value\""));
        assert!(items[1].expect.as_deref().unwrap().contains("error"));
    }

    #[test]
    fn groups_by_file_preserving_first_seen_order() {
        let items = vec![item("b.py", "b1"), item("a.py", "a1"), item("b.py", "b2")];
        let groups = group_by_file(&items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "b.py");
        let covers: Vec<&str> = groups[0].1.iter().map(|c| c.covers.as_str()).collect();
        assert_eq!(covers, vec!["b1", "b2"]);
        assert_eq!(groups[1].0, "a.py");
    }
}
