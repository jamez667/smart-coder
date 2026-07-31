//! Turning a reviewer's reply into anchored findings.
//!
//! Parsing is deliberately forgiving about *shape* and strict about *substance*.
//! A small model wraps JSON in a fence, adds a preamble, or writes `"severity":
//! "moderate"` — none of which change what it found, so none of which should
//! discard it. But a finding citing a hunk it was never shown, or a file that is
//! not in the diff, is a hallucination and is dropped: the anchor is the only
//! thing that makes a finding actionable, so an invented one is worse than
//! nothing.
//!
//! An unparseable reply yields **no findings**, never an error. A reviewer that
//! babbles is a reviewer that found nothing, not a failed review — the same
//! posture as an unreachable reviewer (spec 16).

use crate::diff::{HunkId, IntegratedDiff};
use crate::finding::{Anchor, Finding, Lens, ModelId, Severity};

/// Parse one reviewer's reply for `lens` into findings, keeping only those whose
/// anchor resolves against `diff`.
pub fn parse_findings(
    lens: Lens,
    reply: &str,
    diff: &IntegratedDiff,
    reviewer: &ModelId,
) -> Vec<Finding> {
    let Some(array) = sc_core::extract_json_array(reply) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(array)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| one_finding(lens, v, diff, reviewer))
        .collect()
}

fn one_finding(
    lens: Lens,
    v: &serde_json::Value,
    diff: &IntegratedDiff,
    reviewer: &ModelId,
) -> Option<Finding> {
    let obj = v.as_object()?;
    let summary = str_field(obj, "summary")
        .or_else(|| str_field(obj, "finding"))
        .or_else(|| str_field(obj, "description"))?;
    if summary.trim().is_empty() {
        return None;
    }

    let hunk = str_field(obj, "hunk").and_then(|h| parse_hunk_id(&h));
    let named_file = str_field(obj, "file").map(|f| f.replace('\\', "/"));

    // Resolve the anchor's file. A finding must land on a file that is actually in
    // the diff: everything else is a hallucination with nowhere to point.
    let file = resolve_file(diff, named_file.as_deref(), hunk)?;
    let file_diff = diff.file(&file)?;

    // A cited hunk must exist in THAT file. A model that names a real file and an
    // id from a different one has not selected from what it was shown.
    let hunk = hunk.filter(|h| file_diff.hunks.iter().any(|x| x.id == *h));

    let mut anchor = Anchor::file(file);
    if let Some(h) = hunk {
        anchor = anchor.with_hunk(h);
        // The rendering hint comes from the hunk we resolved, not from whatever
        // line the model claimed — the diff knows where its own hunks start.
        if let Some(hk) = file_diff.hunks.iter().find(|x| x.id == h) {
            anchor = anchor.with_line(hk.new_start);
        }
    }
    if let Some(sym) = str_field(obj, "symbol").filter(|s| !s.trim().is_empty()) {
        anchor = anchor.with_symbol(sym.trim());
    }

    // A finding with neither a hunk nor a symbol identifies nothing beyond a
    // filename. It still attaches to the file (degrading gracefully, per spec) —
    // it simply cannot merge with anything, which `points_at_same_place` enforces.
    let severity = str_field(obj, "severity")
        .and_then(|s| Severity::parse(&s))
        .unwrap_or(Severity::Low);

    Some(Finding::new(
        lens,
        severity,
        anchor,
        summary.trim(),
        reviewer.clone(),
    ))
}

/// Which file a finding is about: the one it named if that file is in the diff,
/// else the file owning the hunk it cited, else — when the diff touches exactly
/// one file — that file. `None` when the claim can't be placed at all.
fn resolve_file(
    diff: &IntegratedDiff,
    named: Option<&str>,
    hunk: Option<HunkId>,
) -> Option<String> {
    if let Some(n) = named {
        if diff.file(n).is_some() {
            return Some(n.to_string());
        }
        // Models shorten paths ("render.rs" for "src/report/render.rs"). Accept a
        // unique suffix match; an ambiguous one is not a resolution.
        let mut matches = diff
            .files
            .iter()
            .filter(|f| f.path.ends_with(n) || n.ends_with(&f.path));
        if let (Some(f), None) = (matches.next(), matches.next()) {
            return Some(f.path.clone());
        }
    }
    if let Some(h) = hunk {
        let mut owners = diff
            .files
            .iter()
            .filter(|f| f.hunks.iter().any(|x| x.id == h));
        if let (Some(f), None) = (owners.next(), owners.next()) {
            return Some(f.path.clone());
        }
    }
    if diff.files.len() == 1 {
        return Some(diff.files[0].path.clone());
    }
    None
}

/// `"H2"`, `"h2"` or `"2"` → `HunkId(2)`.
fn parse_hunk_id(s: &str) -> Option<HunkId> {
    let t = s.trim();
    let digits = t.strip_prefix(['H', 'h']).unwrap_or(t);
    digits.trim().parse::<usize>().ok().map(HunkId)
}

fn str_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    match obj.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff() -> IntegratedDiff {
        IntegratedDiff::from_changes([
            (
                "src/report/render.rs",
                Some("fn render() {}\n"),
                Some("fn render() {}\nfn format_date() {}\n"),
            ),
            (
                "src/other.rs",
                Some("fn o() {}\n"),
                Some("fn o() { x(); }\n"),
            ),
        ])
    }

    fn qwen() -> ModelId {
        ModelId::new("qwen")
    }

    #[test]
    fn parses_a_well_formed_finding_with_its_anchor() {
        let reply = r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                        "severity":"high","summary":"reimplements the date helper"}]"#;
        let f = parse_findings(Lens::Duplication, reply, &diff(), &qwen());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].anchor.file, "src/report/render.rs");
        assert_eq!(f[0].anchor.hunk, Some(HunkId(0)));
        assert_eq!(f[0].anchor.symbol.as_deref(), Some("format_date"));
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].raised_by, vec![qwen()]);
        // Never corroborated straight out of the model — that takes a real check.
        assert!(!f[0].corroborated);
        assert!(f[0].evidence.is_none());
    }

    #[test]
    fn finding_nothing_parses_as_nothing() {
        assert!(parse_findings(Lens::Duplication, "[]", &diff(), &qwen()).is_empty());
    }

    #[test]
    fn a_fenced_or_chatty_reply_still_yields_its_findings() {
        // Small models wrap JSON in fences and add preambles. Neither changes what
        // was found, so neither may discard it.
        let reply = "Sure! Here's what I found:\n```json\n\
            [{\"hunk\":\"H0\",\"file\":\"src/other.rs\",\"severity\":\"low\",\
            \"summary\":\"drive-by\"}]\n```\nHope that helps.";
        let f = parse_findings(Lens::UnrelatedChanges, reply, &diff(), &qwen());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].anchor.file, "src/other.rs");
    }

    #[test]
    fn a_babbling_reviewer_found_nothing_rather_than_failing() {
        for reply in ["I am unable to review this.", "", "{not json"] {
            assert!(
                parse_findings(Lens::ErrorHandling, reply, &diff(), &qwen()).is_empty(),
                "{reply:?}"
            );
        }
    }

    #[test]
    fn a_finding_pointing_at_a_file_outside_the_diff_is_dropped() {
        // The anchor is the only thing that makes a finding actionable, so an
        // invented one is worse than no finding at all.
        let reply = r#"[{"hunk":"H0","file":"src/imaginary.rs","summary":"made up"}]"#;
        assert!(parse_findings(Lens::Duplication, reply, &diff(), &qwen()).is_empty());
    }

    #[test]
    fn a_hunk_id_from_the_wrong_file_is_discarded_but_the_finding_survives() {
        // Both files have an H0 (ids are per-file). A model naming other.rs but
        // citing a hunk it doesn't have keeps the file anchor and loses the hunk.
        let reply = r#"[{"hunk":"H7","file":"src/other.rs","symbol":"o","summary":"x"}]"#;
        let f = parse_findings(Lens::AbstractionFit, reply, &diff(), &qwen());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].anchor.hunk, None);
        assert_eq!(f[0].anchor.symbol.as_deref(), Some("o"));
    }

    #[test]
    fn the_render_line_comes_from_the_hunk_not_from_the_model() {
        // Models cite lines badly. Even if one volunteers a line, the hint is
        // resolved from the hunk the diff actually holds.
        let reply = r#"[{"hunk":"H0","file":"src/report/render.rs","line":999,"summary":"x"}]"#;
        let f = parse_findings(Lens::Duplication, reply, &diff(), &qwen());
        assert_eq!(f[0].anchor.line, Some(2), "line 2 is where H0 starts");
    }

    #[test]
    fn a_shortened_path_resolves_when_unambiguous() {
        let reply = r#"[{"hunk":"H0","file":"render.rs","summary":"x"}]"#;
        let f = parse_findings(Lens::Duplication, reply, &diff(), &qwen());
        assert_eq!(f[0].anchor.file, "src/report/render.rs");
    }

    #[test]
    fn a_summary_less_finding_is_not_a_finding() {
        let reply = r#"[{"hunk":"H0","file":"src/other.rs","severity":"high"},
                        {"hunk":"H0","file":"src/other.rs","summary":"   "}]"#;
        assert!(parse_findings(Lens::ErrorHandling, reply, &diff(), &qwen()).is_empty());
    }

    #[test]
    fn an_unparseable_severity_defaults_low_rather_than_dropping_the_finding() {
        let reply = r#"[{"hunk":"H0","file":"src/other.rs","severity":"spicy","summary":"x"}]"#;
        let f = parse_findings(Lens::ErrorHandling, reply, &diff(), &qwen());
        assert_eq!(f[0].severity, Severity::Low);
    }

    #[test]
    fn hunk_ids_parse_in_the_forms_models_write_them() {
        assert_eq!(parse_hunk_id("H2"), Some(HunkId(2)));
        assert_eq!(parse_hunk_id("h2"), Some(HunkId(2)));
        assert_eq!(parse_hunk_id(" 2 "), Some(HunkId(2)));
        assert_eq!(parse_hunk_id("hunk two"), None);
    }
}
