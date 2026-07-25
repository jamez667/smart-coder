//! The `on_no_files` family — the highest-value lints in the crate.
//!
//! `on_no_files` answers "what does it mean that we could not look at all?" and
//! it defaults to `on_no_match`, which is the single most dangerous default in
//! the pack format. When it is wrong, the report claims "we looked and it wasn't
//! there" when the truth is "we never looked" — a compliance tool lying without
//! anyone deciding to lie.
//!
//! The canonical trap: a `json-path` check against `.github/settings.yml` for
//! branch protection. That file is normally absent because the setting lives in
//! the VCS provider's API. Left to default, its absence becomes a `gap` — a
//! confident, wrong claim that code review is unenforced.

use sc_comply::pack::{Check, CheckKind};
use sc_comply::status::{Outcome, Severity};

use super::{locus, LintCtx, LintFinding};

/// Paths that are conventionally absent from a repository because the thing they
/// configure lives in a provider's API or console. A check targeting one of
/// these MUST set `on_no_files` explicitly.
const PROVIDER_SIDE_PATHS: &[&str] = &[
    ".github/settings.yml",
    ".github/settings.json",
    ".github/branch-protection.json",
    "branch-protection.json",
    ".gitlab/settings.yml",
];

/// The check kinds whose `on_no_files` can actually fire.
///
/// `file-exists` and `file-absent` always look — a path either exists or does
/// not — so `on_no_files` is meaningless for them and flagging it would be noise.
fn can_be_indeterminate(kind: &CheckKind) -> bool {
    matches!(
        kind,
        CheckKind::RegexMatchInGlob { .. }
            | CheckKind::RegexMustNotMatch { .. }
            | CheckKind::SymbolExists { .. }
            | CheckKind::TomlPath { .. }
            | CheckKind::JsonPath { .. }
    )
}

/// The literal path a structured-data check reads, if any.
fn structured_path(kind: &CheckKind) -> Option<&str> {
    match kind {
        CheckKind::TomlPath { path, .. } | CheckKind::JsonPath { path, .. } => Some(path),
        _ => None,
    }
}

pub fn run(ctx: &LintCtx<'_>, out: &mut Vec<LintFinding>) {
    for (control, check) in ctx.checks() {
        let at = locus(control, check);
        provider_side_path_needs_explicit_unknown(check, &at, out);
        absent_in_sample_without_on_no_files(ctx, check, &at, out);
        symbol_check_without_on_no_files(check, &at, out);
        all_outcomes_identical(check, &at, out);
        indeterminate_maps_to_pass(check, &at, out);
    }
}

/// A check reading a path that conventionally lives provider-side, with
/// `on_no_files` left to default.
fn provider_side_path_needs_explicit_unknown(check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let Some(path) = structured_path(&check.kind) else {
        return;
    };
    if !PROVIDER_SIDE_PATHS.contains(&path) {
        return;
    }
    if check.on_no_files.is_some() {
        return;
    }
    out.push(LintFinding::new(
        "provider-side-path-without-on-no-files",
        Severity::High,
        at,
        format!("`{path}` is normally absent because the setting lives in the VCS provider's API, but `on_no_files` is unset"),
        format!(
            "Its absence falls through to on_no_match = {:?}, so the report will claim the control fails when in truth it was never observable from source.",
            check.on_no_match
        ),
        "Set `on_no_files = \"unknown\"` and put the manual step in the rationale (e.g. \"auditor must obtain a settings export from the provider\").",
    ));
}

/// A structured-data check whose target file is absent from the sample, with
/// `on_no_files` left to default.
///
/// Weaker evidence than the provider-side list — the file might simply be
/// missing from this particular repo — so it is medium, and it only fires when a
/// sample was actually supplied.
fn absent_in_sample_without_on_no_files(
    ctx: &LintCtx<'_>,
    check: &Check,
    at: &str,
    out: &mut Vec<LintFinding>,
) {
    let Some(sample) = ctx.sample else {
        return;
    };
    let Some(path) = structured_path(&check.kind) else {
        return;
    };
    // Already reported, with a better explanation.
    if PROVIDER_SIDE_PATHS.contains(&path) {
        return;
    }
    if check.on_no_files.is_some() || sample.has_path(path) {
        return;
    }
    out.push(LintFinding::new(
        "absent-target-without-on-no-files",
        Severity::Medium,
        at,
        format!("`{path}` does not exist in the sample workspace and `on_no_files` is unset"),
        "If this file is commonly absent, every audit of such a repo will report a definite outcome for something that was never read.".to_string(),
        "Decide explicitly: if absence means the control genuinely fails, keep the default and say so in the rationale; if it means we could not see the evidence, set `on_no_files = \"unknown\"`.",
    ));
}

/// A `symbol-exists` check without `on_no_files`.
///
/// `sc-index` parses Rust, Python and C# only, so in a Go or TypeScript repo the
/// collector returns "could not determine" — and defaulting that to a gap
/// reports a false negative on every codebase in an unsupported language.
fn symbol_check_without_on_no_files(check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    if !matches!(check.kind, CheckKind::SymbolExists { .. }) {
        return;
    }
    if check.on_no_files.is_some() {
        return;
    }
    out.push(LintFinding::new(
        "symbol-check-without-on-no-files",
        Severity::High,
        at,
        "`symbol-exists` has no `on_no_files`, but symbol extraction supports only Rust, Python and C#",
        format!(
            "In a repo written in any other language the collector cannot look at all, and that falls through to on_no_match = {:?} — a confident claim about a codebase that was never parsed.",
            check.on_no_match
        ),
        "Set `on_no_files = \"unknown\"`; an unsupported language is a blind spot, not a finding.",
    ));
}

/// Every outcome slot resolves the same way, so the check cannot influence its
/// control.
fn all_outcomes_identical(check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let effective_no_files = check.on_no_files.unwrap_or(check.on_no_match);
    if check.on_match != check.on_no_match || check.on_no_match != effective_no_files {
        return;
    }
    // A deliberately-unconditional `unknown` is a legitimate pattern: it is how a
    // pack declares an organizational control present-but-undeterminable rather
    // than omitting it. Only flag the ones that actually mislead.
    if check.on_match == Outcome::Unknown {
        return;
    }
    out.push(LintFinding::new(
        "all-outcomes-identical",
        Severity::Medium,
        at,
        format!(
            "every outcome resolves to {:?}, so this check's result cannot affect the control",
            check.on_match
        ),
        "The check still runs and still appears in the report, implying evidence was weighed when the answer was fixed in advance.".to_string(),
        "Either give the outcomes different meanings, or delete the check. If the intent is \"present but undeterminable\", map all three to `unknown` — that reads honestly.",
    ));
}

/// "We could not look" maps to `pass`.
///
/// This is the inversion of the crate's governing principle: an unobservable
/// control being reported as satisfied is a false attestation, and it is worse
/// than any other misconfiguration the linter can find.
///
/// The distinction that makes this lint usable rather than noisy: for a negative
/// scan (`regex-must-not-match`), `on_no_match = "pass"` is *correct* — searching
/// files and finding no secrets is exactly the good outcome. The defect is only
/// ever in the `on_no_files` slot, where "we searched nothing" would inherit that
/// same `pass`. So the two cases are reported with different severities and
/// different wording, because they are different mistakes.
fn indeterminate_maps_to_pass(check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    if !can_be_indeterminate(&check.kind) {
        return;
    }
    let effective = check.on_no_files.unwrap_or(check.on_no_match);
    if effective != Outcome::Pass {
        return;
    }

    if check.on_no_files.is_some() {
        // Explicitly written. Whoever typed `on_no_files = "pass"` meant it, and
        // it is indefensible: nothing observed can evidence a control.
        out.push(LintFinding::new(
            "indeterminate-maps-to-pass",
            Severity::Critical,
            at,
            "`on_no_files` is explicitly `pass`, so being unable to look counts as evidence the control is satisfied",
            "A false attestation: the report shows a green control that nothing ever verified. This is the exact failure the Unknown status exists to prevent.".to_string(),
            "Set `on_no_files = \"unknown\"`. Nothing that was never observed may resolve to `pass`.",
        ));
        return;
    }

    // Reached through the default. For a negative scan this is an easy oversight
    // rather than a wrong belief — `on_no_match = "pass"` is right, and the
    // author simply did not consider the zero-files case.
    let negative_scan = matches!(check.kind, CheckKind::RegexMustNotMatch { .. });
    let (severity, summary) = if negative_scan {
        (
            Severity::High,
            "`on_no_match = \"pass\"` is right for a negative scan, but `on_no_files` inherits it — so matching zero files also reports `pass`",
        )
    } else {
        (
            Severity::Critical,
            "`on_no_files` defaults to `on_no_match`, which is `pass`, so being unable to look counts as evidence the control is satisfied",
        )
    };

    out.push(LintFinding::new(
        "indeterminate-maps-to-pass",
        severity,
        at,
        summary,
        "If the glob ever selects nothing — an empty repo, a language this pack does not cover, a renamed directory — the control goes green having read no files at all.".to_string(),
        "Set `on_no_files = \"unknown\"` explicitly. Keep `on_no_match = \"pass\"`: searching files and finding nothing is a genuine pass; searching no files is not.",
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint::lint_pack;
    use crate::test_support::{check_with, pack_of, sample_with};

    fn lints_of(pack_src: &str, lint: &'static str) -> Vec<LintFinding> {
        let pack = sc_comply::Pack::from_toml_str(pack_src).expect("pack parses");
        lint_pack(&pack, None)
            .findings
            .into_iter()
            .filter(|f| f.lint == lint)
            .collect()
    }

    #[test]
    fn flags_provider_side_path_without_on_no_files() {
        let src = pack_of(&check_with(
            "pr-review",
            r#"kind = "json-path"
  path = ".github/settings.yml"
  key_path = "branches.0.protection"
  assert = { kind = "exists" }
  on_match = "pass"
  on_no_match = "gap""#,
        ));
        let found = lints_of(&src, "provider-side-path-without-on-no-files");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::High);
    }

    #[test]
    fn accepts_provider_side_path_with_explicit_unknown() {
        // The shipped SOC 2 pack does exactly this; it must not be flagged.
        let src = pack_of(&check_with(
            "pr-review",
            r#"kind = "json-path"
  path = ".github/settings.yml"
  key_path = "branches.0.protection"
  assert = { kind = "exists" }
  on_match = "pass"
  on_no_match = "gap"
  on_no_files = "unknown""#,
        ));
        assert!(lints_of(&src, "provider-side-path-without-on-no-files").is_empty());
    }

    #[test]
    fn flags_symbol_check_without_on_no_files() {
        let src = pack_of(&check_with(
            "err",
            r#"kind = "symbol-exists"
  name_pattern = "handle_error"
  on_match = "pass"
  on_no_match = "gap""#,
        ));
        let found = lints_of(&src, "symbol-check-without-on-no-files");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, Severity::High);
    }

    #[test]
    fn accepts_symbol_check_with_on_no_files() {
        let src = pack_of(&check_with(
            "err",
            r#"kind = "symbol-exists"
  name_pattern = "handle_error"
  on_match = "pass"
  on_no_match = "unknown"
  on_no_files = "unknown""#,
        ));
        assert!(lints_of(&src, "symbol-check-without-on-no-files").is_empty());
    }

    #[test]
    fn indeterminate_mapping_to_pass_is_critical() {
        // The worst thing a pack can do.
        let src = pack_of(&check_with(
            "tls",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.yml"
  pattern = "min_tls"
  on_match = "pass"
  on_no_match = "gap"
  on_no_files = "pass""#,
        ));
        let found = lints_of(&src, "indeterminate-maps-to-pass");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, Severity::Critical);
    }

    #[test]
    fn catches_pass_reached_through_the_default_on_a_negative_scan() {
        // `on_no_match = "pass"` is CORRECT here — searching and finding no
        // secret is a genuine pass. The defect is only that `on_no_files`
        // inherits it, so searching zero files also reports pass. High, not
        // critical: an oversight rather than a wrong belief.
        let src = pack_of(&check_with(
            "tls",
            r#"kind = "regex-must-not-match"
  glob = "**/*.rs"
  pattern = "verify\\s*=\\s*False"
  on_match = "gap"
  on_no_match = "pass""#,
        ));
        let found = lints_of(&src, "indeterminate-maps-to-pass");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, Severity::High);
        assert!(
            found[0].summary.contains("on_no_files` inherits"),
            "{}",
            found[0].summary
        );
    }

    #[test]
    fn an_explicit_on_no_files_pass_is_critical() {
        // Someone typed it deliberately. Indefensible at any severity below
        // critical: nothing observed cannot evidence a control.
        let src = pack_of(&check_with(
            "tls",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.yml"
  pattern = "min_tls"
  on_match = "pass"
  on_no_match = "gap"
  on_no_files = "pass""#,
        ));
        let found = lints_of(&src, "indeterminate-maps-to-pass");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, Severity::Critical);
        assert!(
            found[0].summary.contains("explicitly"),
            "{}",
            found[0].summary
        );
    }

    #[test]
    fn a_positive_check_defaulting_to_pass_is_critical() {
        // A `regex-match-in-glob` whose on_no_match is pass means "found no
        // evidence, therefore satisfied" — a wrong belief, not an oversight.
        let src = pack_of(&check_with(
            "tls",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.yml"
  pattern = "min_tls"
  on_match = "pass"
  on_no_match = "pass""#,
        ));
        let found = lints_of(&src, "indeterminate-maps-to-pass");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, Severity::Critical);
    }

    #[test]
    fn file_exists_mapping_to_pass_is_not_flagged() {
        // file-exists always looks — there is no indeterminate case, so
        // on_no_match = "pass" is legitimate and must not be noise.
        let src = pack_of(&check_with(
            "ci",
            r#"kind = "file-absent"
  path = ".env"
  on_match = "gap"
  on_no_match = "pass""#,
        ));
        assert!(lints_of(&src, "indeterminate-maps-to-pass").is_empty());
    }

    #[test]
    fn flags_a_check_whose_outcomes_are_all_the_same() {
        let src = pack_of(&check_with(
            "noop",
            r#"kind = "file-exists"
  paths = ["README.md"]
  on_match = "gap"
  on_no_match = "gap""#,
        ));
        let found = lints_of(&src, "all-outcomes-identical");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_unconditional_unknown_is_legitimate() {
        // How a pack declares an organizational control present-but-undeterminable
        // rather than omitting it. The shipped pack does this for CC9.2.
        let src = pack_of(&check_with(
            "vendors",
            r#"kind = "file-exists"
  paths = ["VENDORS.md"]
  on_match = "unknown"
  on_no_match = "unknown""#,
        ));
        assert!(lints_of(&src, "all-outcomes-identical").is_empty());
    }

    #[test]
    fn absent_target_lint_needs_a_sample_and_stays_quiet_without_one() {
        let src = pack_of(&check_with(
            "cfg",
            r#"kind = "toml-path"
  path = "nonexistent.toml"
  key_path = "a.b"
  assert = { kind = "exists" }
  on_match = "pass"
  on_no_match = "gap""#,
        ));
        // No sample: must not fire.
        assert!(lints_of(&src, "absent-target-without-on-no-files").is_empty());

        // With a sample that lacks the file: fires.
        let pack = sc_comply::Pack::from_toml_str(&src).expect("pack");
        let sample = sample_with(&["src/lib.rs"]);
        let found: Vec<_> = lint_pack(&pack, Some(&sample))
            .findings
            .into_iter()
            .filter(|f| f.lint == "absent-target-without-on-no-files")
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_present_target_is_not_flagged() {
        let src = pack_of(&check_with(
            "cfg",
            r#"kind = "toml-path"
  path = "Cargo.toml"
  key_path = "package.name"
  assert = { kind = "exists" }
  on_match = "pass"
  on_no_match = "gap""#,
        ));
        let pack = sc_comply::Pack::from_toml_str(&src).expect("pack");
        let sample = sample_with(&["Cargo.toml"]);
        let found: Vec<_> = lint_pack(&pack, Some(&sample))
            .findings
            .into_iter()
            .filter(|f| f.lint == "absent-target-without-on-no-files")
            .collect();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_well_formed_check_trips_nothing_here() {
        // The false-positive guard: a linter that cries wolf gets switched off.
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*.rs"
  pattern = "BEGIN RSA PRIVATE KEY"
  exclude_globs = ["tests/**/*"]
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        let pack = sc_comply::Pack::from_toml_str(&src).expect("pack");
        let sample = sample_with(&["src/lib.rs"]);
        let report = lint_pack(&pack, Some(&sample));
        let from_here: Vec<_> = report
            .findings
            .iter()
            .filter(|f| {
                [
                    "provider-side-path-without-on-no-files",
                    "absent-target-without-on-no-files",
                    "symbol-check-without-on-no-files",
                    "all-outcomes-identical",
                    "indeterminate-maps-to-pass",
                ]
                .contains(&f.lint)
            })
            .collect();
        assert!(from_here.is_empty(), "{from_here:?}");
    }
}
