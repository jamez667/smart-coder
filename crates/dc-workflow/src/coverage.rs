//! The Phase-4 test plan (spec 09, amended): the orchestrator lists the *coverage*
//! each test file must hit; small worker models then write the actual tests.
//!
//! This keeps the architect doing what it's good at (deciding what behavior to
//! pin) and the cheap workers doing the writing — the same labour split as the
//! implementation swarm (spec 08).

use std::collections::BTreeMap;

use dc_core::extract_json_array;
use serde::{Deserialize, Serialize};

/// One behavior a test must check, scoped to a test file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageItem {
    pub file: String,
    pub covers: String,
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
        let file = item
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
        if !file.is_empty() && !covers.is_empty() {
            out.push(CoverageItem { file, covers });
        }
    }
    out
}

/// Group coverage items by test file, preserving the order each file first
/// appears. One group → one test-writing worker (file-granularity rule, spec 08).
pub fn group_by_file(items: &[CoverageItem]) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for it in items {
        if !map.contains_key(&it.file) {
            order.push(it.file.clone());
        }
        map.entry(it.file.clone())
            .or_default()
            .push(it.covers.clone());
    }
    order
        .into_iter()
        .map(|f| {
            let covers = map.remove(&f).unwrap_or_default();
            (f, covers)
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
    fn tolerates_prose_and_skips_incomplete() {
        let reply =
            "Here is the plan:\n[{\"file\":\"t.py\",\"covers\":\"x\"},{\"file\":\"t.py\"}]\ndone";
        let items = parse_coverage(reply);
        assert_eq!(items.len(), 1); // the item missing `covers` is dropped
    }

    #[test]
    fn groups_by_file_preserving_first_seen_order() {
        let items = vec![
            CoverageItem {
                file: "b.py".into(),
                covers: "b1".into(),
            },
            CoverageItem {
                file: "a.py".into(),
                covers: "a1".into(),
            },
            CoverageItem {
                file: "b.py".into(),
                covers: "b2".into(),
            },
        ];
        let groups = group_by_file(&items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "b.py");
        assert_eq!(groups[0].1, vec!["b1", "b2"]);
        assert_eq!(groups[1].0, "a.py");
    }
}
