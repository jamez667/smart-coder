//! Rendering drafted checks as house-style pack TOML.
//!
//! We render rather than asking the model for TOML so drafts match the shipped
//! pack's formatting, and so provenance markers are guaranteed present rather
//! than dependent on the model remembering to add them.
//!
//! **Provenance is not optional.** An auditor reading a pack must be able to
//! tell a human-authored check from a machine-drafted one, because they carry
//! different weight. The marker is a comment, so it survives review-and-edit and
//! disappears only when a human deliberately deletes it — an explicit act of
//! taking ownership of the check.

use std::fmt::Write as _;

use sc_comply::pack::CheckKind;
use sc_comply::status::Outcome;

use super::parse::DraftCheck;

/// How a drafted check is attributed in the emitted TOML.
#[derive(Debug, Clone)]
pub struct Provenance {
    /// The model that produced it, e.g. `"gemini-2.5-pro"`.
    pub model: String,
    /// RFC3339 timestamp. Injected so rendering is deterministic in tests.
    pub generated_at: String,
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Pass => "pass",
        Outcome::Gap => "gap",
        Outcome::Unknown => "unknown",
        Outcome::NotApplicable => "not-applicable",
    }
}

/// TOML-escape a string for a basic double-quoted value.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render one drafted check as a `[[controls.checks]]` block.
pub fn render_check(check: &DraftCheck, prov: &Provenance) -> String {
    let mut s = String::with_capacity(512);

    let _ = writeln!(s, "  [[controls.checks]]");
    let _ = writeln!(s, "  id = \"{}\"", esc(&check.id));
    // The marker sits directly under the id so it cannot be missed when
    // skim-reading a diff.
    let _ = writeln!(
        s,
        "  # DRAFT ({}, {}) — REVIEW BEFORE USE.",
        prov.model, prov.generated_at
    );
    let _ = writeln!(
        s,
        "  # Verify on_no_files: does absence mean \"not configured\", or \"we could not see it\"?"
    );

    render_kind(&mut s, &check.kind);

    if !check.exclude_globs.is_empty() {
        let globs = check
            .exclude_globs
            .iter()
            .map(|g| format!("\"{}\"", esc(g)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(s, "  exclude_globs = [{globs}]");
    }

    let _ = writeln!(s, "  on_match = \"{}\"", outcome_str(check.on_match));
    let _ = writeln!(s, "  on_no_match = \"{}\"", outcome_str(check.on_no_match));
    if let Some(o) = check.on_no_files {
        let _ = writeln!(s, "  on_no_files = \"{}\"", outcome_str(o));
    }
    if let Some(w) = check.weight {
        let _ = writeln!(s, "  weight = {w}");
    }
    if !check.rationale.trim().is_empty() {
        let _ = writeln!(s, "  rationale = \"\"\"");
        for line in check.rationale.trim().lines() {
            let _ = writeln!(s, "{line}");
        }
        let _ = writeln!(s, "\"\"\"");
    }

    s
}

fn render_kind(s: &mut String, kind: &CheckKind) {
    match kind {
        CheckKind::FileExists { paths } => {
            let list = paths
                .iter()
                .map(|p| format!("\"{}\"", esc(p)))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(s, "  kind = \"file-exists\"");
            let _ = writeln!(s, "  paths = [{list}]");
        }
        CheckKind::FileAbsent { path } => {
            let _ = writeln!(s, "  kind = \"file-absent\"");
            let _ = writeln!(s, "  path = \"{}\"", esc(path));
        }
        CheckKind::RegexMatchInGlob { glob, pattern } => {
            let _ = writeln!(s, "  kind = \"regex-match-in-glob\"");
            let _ = writeln!(s, "  glob = \"{}\"", esc(glob));
            let _ = writeln!(s, "  pattern = \"{}\"", esc(pattern));
        }
        CheckKind::RegexMustNotMatch { glob, pattern } => {
            let _ = writeln!(s, "  kind = \"regex-must-not-match\"");
            let _ = writeln!(s, "  glob = \"{}\"", esc(glob));
            let _ = writeln!(s, "  pattern = \"{}\"", esc(pattern));
        }
        CheckKind::SymbolExists {
            name_pattern,
            languages,
        } => {
            let _ = writeln!(s, "  kind = \"symbol-exists\"");
            let _ = writeln!(s, "  name_pattern = \"{}\"", esc(name_pattern));
            if !languages.is_empty() {
                let langs = languages
                    .iter()
                    .map(|l| {
                        let n = match l {
                            sc_comply::pack::LangSel::Rust => "rust",
                            sc_comply::pack::LangSel::Python => "python",
                            sc_comply::pack::LangSel::CSharp => "csharp",
                        };
                        format!("\"{n}\"")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(s, "  languages = [{langs}]");
            }
        }
        CheckKind::TomlPath {
            path,
            key_path,
            assert,
        } => {
            let _ = writeln!(s, "  kind = \"toml-path\"");
            let _ = writeln!(s, "  path = \"{}\"", esc(path));
            let _ = writeln!(s, "  key_path = \"{}\"", esc(key_path));
            let _ = writeln!(s, "  assert = {}", render_assert(assert));
        }
        CheckKind::JsonPath {
            path,
            key_path,
            assert,
        } => {
            let _ = writeln!(s, "  kind = \"json-path\"");
            let _ = writeln!(s, "  path = \"{}\"", esc(path));
            let _ = writeln!(s, "  key_path = \"{}\"", esc(key_path));
            let _ = writeln!(s, "  assert = {}", render_assert(assert));
        }
        CheckKind::CommandExitCode {
            command,
            expect_codes,
            timeout_secs,
        } => {
            let codes = expect_codes
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(s, "  kind = \"command-exit-code\"");
            let _ = writeln!(s, "  command = \"{}\"", esc(command));
            let _ = writeln!(s, "  expect_codes = [{codes}]");
            let _ = writeln!(s, "  timeout_secs = {timeout_secs}");
        }
    }
}

fn render_assert(a: &sc_comply::pack::Assertion) -> String {
    use sc_comply::pack::Assertion as A;
    match a {
        A::Exists => "{ kind = \"exists\" }".to_string(),
        A::NonEmpty => "{ kind = \"non-empty\" }".to_string(),
        A::Equals { value } => format!(
            "{{ kind = \"equals\", value = {} }}",
            render_toml_value(value)
        ),
        A::NotEquals { value } => format!(
            "{{ kind = \"not-equals\", value = {} }}",
            render_toml_value(value)
        ),
        A::Gte { value } => format!("{{ kind = \"gte\", value = {value} }}"),
        A::Lte { value } => format!("{{ kind = \"lte\", value = {value} }}"),
        A::Matches { pattern } => {
            format!("{{ kind = \"matches\", pattern = \"{}\" }}", esc(pattern))
        }
    }
}

fn render_toml_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => format!("\"{}\"", esc(s)),
        other => other.to_string(),
    }
}

/// Render a whole control block around its drafted checks.
#[allow(clippy::too_many_arguments)]
pub fn render_control(
    id: &str,
    title: &str,
    clause: &str,
    intent: &str,
    severity: &str,
    checks: &[DraftCheck],
    prov: &Provenance,
) -> String {
    let mut s = String::with_capacity(2048);
    let _ = writeln!(s, "[[controls]]");
    let _ = writeln!(s, "id = \"{}\"", esc(id));
    let _ = writeln!(s, "title = \"{}\"", esc(title));
    if !clause.is_empty() {
        let _ = writeln!(s, "clause = \"{}\"", esc(clause));
    }
    let _ = writeln!(s, "severity = \"{severity}\"");
    if !intent.trim().is_empty() {
        let _ = writeln!(s, "intent = \"\"\"");
        for line in intent.trim().lines() {
            let _ = writeln!(s, "{line}");
        }
        let _ = writeln!(s, "\"\"\"");
    }
    for c in checks {
        let _ = writeln!(s);
        s.push_str(&render_check(c, prov));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::parse::parse_drafts;

    fn prov() -> Provenance {
        Provenance {
            model: "gemini-test".into(),
            generated_at: "2026-07-25T00:00:00Z".into(),
        }
    }

    fn draft(json: &str) -> DraftCheck {
        let out = parse_drafts(json);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        out.checks.into_iter().next().expect("one check")
    }

    /// Wrap rendered checks in a minimal pack so the real parser validates them.
    fn as_pack(body: &str) -> String {
        format!(
            r#"
[framework]
id = "t"
name = "T"
version = "1"
authority = "A"

[[controls]]
id = "T1"
title = "t"
intent = "i"
{body}
"#
        )
    }

    #[test]
    fn every_drafted_check_carries_a_provenance_marker() {
        let c = draft(
            r#"[{"id":"x","kind":"file-exists","paths":["README.md"],
                 "on_match":"pass","on_no_match":"gap"}]"#,
        );
        let out = render_check(&c, &prov());
        assert!(
            out.contains("# DRAFT (gemini-test, 2026-07-25T00:00:00Z)"),
            "{out}"
        );
        assert!(out.contains("REVIEW BEFORE USE"), "{out}");
        assert!(out.contains("Verify on_no_files"), "{out}");
    }

    #[test]
    fn rendered_toml_round_trips_through_the_real_parser() {
        // The property that matters: what we emit must actually load.
        let c = draft(
            r#"[{"id":"ci","kind":"regex-match-in-glob","glob":"**/*.yml",
                 "pattern":"cargo test","on_match":"pass","on_no_match":"gap",
                 "on_no_files":"unknown","rationale":"CI runs the suite."}]"#,
        );
        let src = as_pack(&render_check(&c, &prov()));
        let pack = sc_comply::Pack::from_toml_str(&src).expect("rendered TOML must parse");
        assert_eq!(pack.controls[0].checks.len(), 1);
        assert_eq!(pack.controls[0].checks[0].id, "ci");
    }

    #[test]
    fn renders_every_check_kind_to_loadable_toml() {
        let cases = [
            r#"[{"id":"a","kind":"file-exists","paths":["A","B"],"on_match":"pass","on_no_match":"gap"}]"#,
            r#"[{"id":"b","kind":"file-absent","path":".env","on_match":"gap","on_no_match":"pass"}]"#,
            r#"[{"id":"c","kind":"regex-match-in-glob","glob":"**/*.rs","pattern":"x","on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
            r#"[{"id":"d","kind":"regex-must-not-match","glob":"**/*.rs","pattern":"y","on_match":"gap","on_no_match":"pass","on_no_files":"unknown"}]"#,
            r#"[{"id":"e","kind":"symbol-exists","name_pattern":"init","languages":["rust","python"],"on_match":"pass","on_no_match":"unknown","on_no_files":"unknown"}]"#,
            r#"[{"id":"f","kind":"toml-path","path":"Cargo.toml","key_path":"package.name","assert":{"kind":"exists"},"on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
            r#"[{"id":"g","kind":"json-path","path":"p.json","key_path":"a.0","assert":{"kind":"gte","value":1},"on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
            r#"[{"id":"h","kind":"command-exit-code","command":"cargo audit","on_match":"pass","on_no_match":"gap"}]"#,
        ];
        for json in cases {
            let c = draft(json);
            let src = as_pack(&render_check(&c, &prov()));
            sc_comply::Pack::from_toml_str(&src)
                .unwrap_or_else(|e| panic!("kind from {json} failed to round-trip: {e}"));
        }
    }

    #[test]
    fn renders_assert_variants() {
        for json in [
            r#"[{"id":"a","kind":"toml-path","path":"p","key_path":"k","assert":{"kind":"equals","value":"2021"},"on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
            r#"[{"id":"b","kind":"toml-path","path":"p","key_path":"k","assert":{"kind":"non-empty"},"on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
            r#"[{"id":"c","kind":"toml-path","path":"p","key_path":"k","assert":{"kind":"matches","pattern":"^v"},"on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
            r#"[{"id":"d","kind":"toml-path","path":"p","key_path":"k","assert":{"kind":"lte","value":5},"on_match":"pass","on_no_match":"gap","on_no_files":"unknown"}]"#,
        ] {
            let c = draft(json);
            let src = as_pack(&render_check(&c, &prov()));
            sc_comply::Pack::from_toml_str(&src)
                .unwrap_or_else(|e| panic!("assert from {json} failed: {e}"));
        }
    }

    #[test]
    fn escapes_quotes_and_backslashes_in_patterns() {
        // Regexes are full of backslashes; a naive renderer corrupts them.
        let c = draft(
            r#"[{"id":"x","kind":"regex-must-not-match","glob":"**/*.rs",
                 "pattern":"verify\\s*=\\s*\"False\"","on_match":"gap",
                 "on_no_match":"pass","on_no_files":"unknown"}]"#,
        );
        let src = as_pack(&render_check(&c, &prov()));
        let pack = sc_comply::Pack::from_toml_str(&src).expect("must parse");
        match &pack.controls[0].checks[0].kind {
            CheckKind::RegexMustNotMatch { pattern, .. } => {
                assert_eq!(pattern, "verify\\s*=\\s*\"False\"");
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn renders_a_full_control_block() {
        let c = draft(
            r#"[{"id":"x","kind":"file-exists","paths":["README.md"],
                 "on_match":"unknown","on_no_match":"unknown","on_no_files":"unknown",
                 "rationale":"Documented, not operating."}]"#,
        );
        let out = render_control(
            "A.5.1",
            "Policies for information security",
            "ISO 27001 A.5.1",
            "Management shall define a set of policies.",
            "medium",
            &[c],
            &prov(),
        );
        let src =
            format!("[framework]\nid=\"t\"\nname=\"T\"\nversion=\"1\"\nauthority=\"A\"\n\n{out}");
        let pack = sc_comply::Pack::from_toml_str(&src).expect("control block must parse");
        assert_eq!(pack.controls[0].id, "A.5.1");
        assert!(pack.controls[0].intent.contains("Management shall"));
    }

    #[test]
    fn the_marker_survives_a_render_parse_render_cycle() {
        // Provenance must not be lost when a pack is re-emitted.
        let c = draft(
            r#"[{"id":"x","kind":"file-exists","paths":["A"],"on_match":"pass","on_no_match":"gap"}]"#,
        );
        let first = render_check(&c, &prov());
        let src = as_pack(&first);
        let pack = sc_comply::Pack::from_toml_str(&src).expect("parse");
        // Re-render from the parsed form and confirm the marker is re-applied.
        let reparsed = DraftCheck {
            id: pack.controls[0].checks[0].id.clone(),
            kind: pack.controls[0].checks[0].kind.clone(),
            on_match: pack.controls[0].checks[0].on_match,
            on_no_match: pack.controls[0].checks[0].on_no_match,
            on_no_files: pack.controls[0].checks[0].on_no_files,
            weight: None,
            exclude_globs: vec![],
            rationale: String::new(),
        };
        assert!(render_check(&reparsed, &prov()).contains("# DRAFT"));
    }
}
