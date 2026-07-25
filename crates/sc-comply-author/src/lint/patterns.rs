//! Regex and glob lints: is this check capable of ever finding anything, and
//! will it find the wrong thing?
//!
//! Two failure modes matter here, and they pull in opposite directions. A check
//! that can *never* match is inert — it reports a definite outcome forever
//! without looking at anything meaningful. A check that matches *too much* hits
//! the pack's own sources and test fixtures, burying the real finding under
//! noise until an auditor stops reading. Both are provable without a model.

use regex::Regex;
use sc_comply::pack::{Check, CheckKind};
use sc_comply::status::Severity;
use sc_comply::Glob;

use super::{locus, LintCtx, LintFinding};

/// Globs that select the tooling's own sources — where a secret detector will
/// always find its own detection patterns and test fixtures.
const SELF_GLOBS: &[&str] = &[
    "crates/sc-comply/src/**/*",
    "crates/sc-comply/packs/*.toml",
    "crates/sc-comply-author/src/**/*",
];

/// The glob and pattern a text check carries.
fn text_parts(kind: &CheckKind) -> Option<(&str, &str)> {
    match kind {
        CheckKind::RegexMatchInGlob { glob, pattern }
        | CheckKind::RegexMustNotMatch { glob, pattern } => Some((glob, pattern)),
        _ => None,
    }
}

pub fn run(ctx: &LintCtx<'_>, out: &mut Vec<LintFinding>) {
    for (control, check) in ctx.checks() {
        let at = locus(control, check);
        look_around_unsupported(check, &at, out);
        glob_matches_nothing(ctx, check, &at, out);
        pattern_never_matches(ctx, check, &at, out);
        self_referential_pattern(ctx, check, &at, out);
        must_not_match_without_exclusions(check, &at, out);
        untracked_only_evidence(ctx, check, &at, out);
    }
}

/// PCRE look-around, which the `regex` crate cannot compile.
///
/// `Pack::validate()` already rejects this, so a pack reaching the linter cannot
/// contain it — but the lint exists for the *drafting* path, where a model's
/// proposal is linted before it is ever rendered to TOML. It is the natural way
/// to write "http:// but not localhost" coming from any other regex flavour.
fn look_around_unsupported(check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let Some((_, pattern)) = text_parts(&check.kind) else {
        return;
    };
    let has_look_around = ["(?=", "(?!", "(?<=", "(?<!"]
        .iter()
        .any(|m| pattern.contains(m));
    if !has_look_around {
        return;
    }
    out.push(LintFinding::new(
        "regex-no-look-around",
        Severity::High,
        at,
        "the pattern uses look-around, which the `regex` crate does not support",
        "The pack will fail to load. Worse, if it were accepted it would silently never match, and a never-matching `must-not-match` reads as a clean pass.".to_string(),
        "Rewrite positively. Instead of \"http:// not followed by localhost\", match a public TLD: `http://[a-z0-9.-]+\\.(com|org|net)\\b`.",
    ));
}

/// The glob selects nothing in the sample workspace.
fn glob_matches_nothing(ctx: &LintCtx<'_>, check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let (Some(sample), Some((glob_src, _))) = (ctx.sample, text_parts(&check.kind)) else {
        return;
    };
    if sample.is_empty() {
        return;
    }
    let Ok(glob) = Glob::new(glob_src) else {
        return; // validate() already covers a malformed glob.
    };
    if !sample.matching(&glob).is_empty() {
        return;
    }
    out.push(LintFinding::new(
        "glob-matches-nothing",
        Severity::Medium,
        at,
        format!("glob `{glob_src}` selects no files in the sample workspace"),
        format!(
            "Against a repo like this one the check is inert: it always reports the `on_no_files` outcome ({:?}) without reading anything.",
            check.on_no_files.unwrap_or(check.on_no_match)
        ),
        "Confirm the glob is right for the repos this pack targets. If the file type is genuinely optional, make sure `on_no_files = \"unknown\"` so the inert case is honest.",
    ));
}

/// The pattern's required literal cannot occur in any file the glob selects.
///
/// Only claims this when it can *prove* it: the pattern must contain a literal
/// run long enough to be meaningful, and no selected file may contain it. That
/// is evidence the check is inert against this repo, which is worth saying
/// without overclaiming that it is inert everywhere.
fn pattern_never_matches(ctx: &LintCtx<'_>, check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let (Some(sample), Some((glob_src, pattern))) = (ctx.sample, text_parts(&check.kind)) else {
        return;
    };
    let (Ok(glob), Ok(re)) = (Glob::new(glob_src), Regex::new(pattern)) else {
        return;
    };
    let selected = sample.matching(&glob);
    // An empty selection is `glob-matches-nothing`'s job, not ours.
    if selected.is_empty() {
        return;
    }
    let matched = selected
        .iter()
        .any(|f| f.contents.lines().any(|l| re.is_match(l)));
    if matched {
        return;
    }
    // Only report for a `regex-match-in-glob` looking for evidence of a control
    // being satisfied. A `must-not-match` finding nothing is the GOOD case — the
    // repo is clean — and flagging it would be actively wrong.
    if !matches!(check.kind, CheckKind::RegexMatchInGlob { .. }) {
        return;
    }
    out.push(LintFinding::new(
        "pattern-matches-nothing-in-sample",
        Severity::Low,
        at,
        format!("pattern never matches across the {} file(s) the glob selects", selected.len()),
        "Either the pattern is too narrow, or this repo genuinely lacks the evidence. The lint cannot tell which, but a pattern that never fires on any real repo is worth re-reading.".to_string(),
        "Test the pattern against a repo you know satisfies the control. If it still does not fire, widen it or reconsider the check.",
    ));
}

/// The pattern matches the tooling's own sources, with no exclusion covering
/// them.
///
/// This is the defect the first self-audit surfaced: a secret detector matches
/// its own detection pattern, so the shipped pack's regexes and this crate's
/// test fixtures dominate the findings list and bury the one real hit.
fn self_referential_pattern(
    ctx: &LintCtx<'_>,
    check: &Check,
    at: &str,
    out: &mut Vec<LintFinding>,
) {
    let (Some(sample), Some((glob_src, pattern))) = (ctx.sample, text_parts(&check.kind)) else {
        return;
    };
    if !matches!(check.kind, CheckKind::RegexMustNotMatch { .. }) {
        return;
    }
    let (Ok(glob), Ok(re)) = (Glob::new(glob_src), Regex::new(pattern)) else {
        return;
    };
    let excludes: Vec<Glob> = check
        .exclude_globs
        .iter()
        .filter_map(|g| Glob::new(g).ok())
        .collect();

    let self_hits: Vec<&str> = sample
        .matching(&glob)
        .into_iter()
        .filter(|f| {
            is_self_path(&f.path)
                && !excludes.iter().any(|e| e.is_match(&f.path))
                && f.contents.lines().any(|l| re.is_match(l))
        })
        .map(|f| f.path.as_str())
        .collect();

    if self_hits.is_empty() {
        return;
    }
    let shown = self_hits
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    out.push(LintFinding::new(
        "self-referential-pattern",
        Severity::High,
        at,
        format!(
            "the pattern matches the tooling's own sources ({} file(s), e.g. {shown})",
            self_hits.len()
        ),
        "These are the detector's own patterns and fixtures, not findings. They crowd out the genuine hit and train the reader to skim past this control.".to_string(),
        "Add the offending paths to `exclude_globs`. Every suppression is disclosed in the evidence pack, so this does not hide anything from an auditor.",
    ));
}

fn is_self_path(path: &str) -> bool {
    SELF_GLOBS
        .iter()
        .filter_map(|g| Glob::new(g).ok())
        .any(|g| g.is_match(path))
}

/// A repo-wide `must-not-match` with no exclusions at all.
fn must_not_match_without_exclusions(check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let Some((glob_src, _)) = text_parts(&check.kind) else {
        return;
    };
    if !matches!(check.kind, CheckKind::RegexMustNotMatch { .. }) {
        return;
    }
    if !check.exclude_globs.is_empty() {
        return;
    }
    // Only the genuinely repo-wide globs; a targeted one is fine unexcluded.
    if !matches!(glob_src, "**/*" | "**") {
        return;
    }
    out.push(LintFinding::new(
        "must-not-match-without-exclusions",
        Severity::Medium,
        at,
        format!("`regex-must-not-match` over `{glob_src}` with no `exclude_globs`"),
        "A repo-wide negative scan almost always hits test fixtures, vendored samples and documentation, none of which are real findings.".to_string(),
        "Add `exclude_globs` for fixtures and vendored trees, or narrow the glob to the file types that can actually carry the problem.",
    ));
}

/// Every hit the check can produce in the sample comes from a gitignored path.
///
/// The finding is real — an untracked secret is still an exposure — but it is
/// not "committed to source", and a control worded as if it were will overstate.
fn untracked_only_evidence(ctx: &LintCtx<'_>, check: &Check, at: &str, out: &mut Vec<LintFinding>) {
    let (Some(sample), Some((glob_src, pattern))) = (ctx.sample, text_parts(&check.kind)) else {
        return;
    };
    let (Ok(glob), Ok(re)) = (Glob::new(glob_src), Regex::new(pattern)) else {
        return;
    };
    let excludes: Vec<Glob> = check
        .exclude_globs
        .iter()
        .filter_map(|g| Glob::new(g).ok())
        .collect();

    let hits: Vec<&sc_comply::scan::TextFile> = sample
        .matching(&glob)
        .into_iter()
        .filter(|f| {
            !excludes.iter().any(|e| e.is_match(&f.path))
                && f.contents.lines().any(|l| re.is_match(l))
        })
        .collect();

    if hits.is_empty() || !hits.iter().all(|f| f.ignored) {
        return;
    }
    out.push(LintFinding::new(
        "untracked-only-evidence",
        Severity::Low,
        at,
        format!(
            "every hit in the sample ({}) comes from a gitignored path",
            hits.len()
        ),
        "The evidence pack labels these `[untracked]`, but a control whose title says \"committed to source\" will still read as a stronger claim than the evidence supports.".to_string(),
        "Check the control's title and intent describe on-disk exposure rather than version-control history, or split the two into separate controls.",
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint::lint_pack;
    use crate::sample::Sample;
    use crate::test_support::{check_with, pack_of, sample_with_contents};

    fn findings(pack_src: &str, sample: Option<&Sample>, lint: &'static str) -> Vec<LintFinding> {
        let pack = sc_comply::Pack::from_toml_str(pack_src).expect("pack parses");
        lint_pack(&pack, sample)
            .findings
            .into_iter()
            .filter(|f| f.lint == lint)
            .collect()
    }

    #[test]
    fn flags_look_around() {
        // Pack::validate rejects this, so build the check directly against the
        // lint rather than through from_toml_str.
        let check = sc_comply::pack::Check {
            id: "x".into(),
            kind: CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "http://(?!localhost)".into(),
            },
            on_match: sc_comply::status::Outcome::Gap,
            on_no_match: sc_comply::status::Outcome::Pass,
            on_no_files: None,
            weight: 1.0,
            exclude_globs: vec![],
            rationale: String::new(),
        };
        let mut out = Vec::new();
        look_around_unsupported(&check, "T1/x", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, Severity::High);
        assert!(out[0].suggestion.contains("positively"));
    }

    #[test]
    fn flags_a_glob_that_selects_nothing() {
        let src = pack_of(&check_with(
            "tf",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.tf"
  pattern = "min_tls_version"
  on_match = "pass"
  on_no_match = "unknown"
  on_no_files = "unknown""#,
        ));
        let sample = sample_with_contents(&[("src/lib.rs", "fn main() {}")]);
        assert_eq!(
            findings(&src, Some(&sample), "glob-matches-nothing").len(),
            1
        );
    }

    #[test]
    fn a_glob_that_selects_files_is_not_flagged() {
        let src = pack_of(&check_with(
            "rs",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.rs"
  pattern = "tracing::"
  on_match = "pass"
  on_no_match = "gap"
  on_no_files = "unknown""#,
        ));
        let sample = sample_with_contents(&[("src/lib.rs", "tracing::info!(\"x\");")]);
        assert!(findings(&src, Some(&sample), "glob-matches-nothing").is_empty());
    }

    #[test]
    fn flags_a_self_referential_must_not_match() {
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*"
  pattern = "BEGIN RSA PRIVATE KEY"
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        let sample = sample_with_contents(&[
            (
                "crates/sc-comply/src/collectors/text.rs",
                "\"BEGIN RSA PRIVATE KEY\"",
            ),
            ("deploy/real.key", "BEGIN RSA PRIVATE KEY"),
        ]);
        let found = findings(&src, Some(&sample), "self-referential-pattern");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].summary.contains("sc-comply"),
            "{}",
            found[0].summary
        );
    }

    #[test]
    fn exclusions_silence_the_self_reference_lint() {
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*"
  pattern = "BEGIN RSA PRIVATE KEY"
  exclude_globs = ["crates/sc-comply/src/**/*"]
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        let sample = sample_with_contents(&[
            (
                "crates/sc-comply/src/collectors/text.rs",
                "\"BEGIN RSA PRIVATE KEY\"",
            ),
            ("deploy/real.key", "BEGIN RSA PRIVATE KEY"),
        ]);
        assert!(findings(&src, Some(&sample), "self-referential-pattern").is_empty());
    }

    #[test]
    fn flags_repo_wide_must_not_match_without_exclusions() {
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*"
  pattern = "SECRET"
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        assert_eq!(
            findings(&src, None, "must-not-match-without-exclusions").len(),
            1
        );
    }

    #[test]
    fn a_narrow_must_not_match_needs_no_exclusions() {
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*.rs"
  pattern = "SECRET"
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        assert!(findings(&src, None, "must-not-match-without-exclusions").is_empty());
    }

    #[test]
    fn a_clean_must_not_match_is_not_reported_as_never_matching() {
        // Finding nothing is the GOOD case for a negative scan. Flagging it
        // would tell an author to loosen a working control.
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
        let sample = sample_with_contents(&[("src/lib.rs", "fn main() {}")]);
        assert!(findings(&src, Some(&sample), "pattern-matches-nothing-in-sample").is_empty());
    }

    #[test]
    fn flags_a_positive_pattern_that_never_fires() {
        let src = pack_of(&check_with(
            "log",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.rs"
  pattern = "winston\\."
  on_match = "pass"
  on_no_match = "gap"
  on_no_files = "unknown""#,
        ));
        let sample = sample_with_contents(&[("src/lib.rs", "fn main() {}")]);
        assert_eq!(
            findings(&src, Some(&sample), "pattern-matches-nothing-in-sample").len(),
            1
        );
    }

    #[test]
    fn flags_evidence_that_is_entirely_untracked() {
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*"
  pattern = "BEGIN EC PRIVATE KEY"
  exclude_globs = ["crates/sc-comply/src/**/*"]
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        let sample = Sample::from_files(
            "/ws",
            vec![sc_comply::scan::TextFile {
                path: "server.key".into(),
                contents: "BEGIN EC PRIVATE KEY".into(),
                ignored: true,
            }],
        );
        assert_eq!(
            findings(&src, Some(&sample), "untracked-only-evidence").len(),
            1
        );
    }

    #[test]
    fn a_tracked_hit_is_not_flagged_as_untracked_only() {
        let src = pack_of(&check_with(
            "keys",
            r#"kind = "regex-must-not-match"
  glob = "**/*"
  pattern = "BEGIN EC PRIVATE KEY"
  exclude_globs = ["crates/sc-comply/src/**/*"]
  on_match = "gap"
  on_no_match = "pass"
  on_no_files = "unknown""#,
        ));
        let sample = sample_with_contents(&[("committed.key", "BEGIN EC PRIVATE KEY")]);
        assert!(findings(&src, Some(&sample), "untracked-only-evidence").is_empty());
    }

    #[test]
    fn lints_that_need_a_sample_stay_quiet_without_one() {
        let src = pack_of(&check_with(
            "x",
            r#"kind = "regex-match-in-glob"
  glob = "**/*.tf"
  pattern = "anything"
  on_match = "pass"
  on_no_match = "gap"
  on_no_files = "unknown""#,
        ));
        for lint in [
            "glob-matches-nothing",
            "pattern-matches-nothing-in-sample",
            "self-referential-pattern",
            "untracked-only-evidence",
        ] {
            assert!(
                findings(&src, None, lint).is_empty(),
                "{lint} fired without a sample"
            );
        }
    }
}
