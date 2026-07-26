//! Line-level regex evidence over glob-selected files.
//!
//! This collector produces most of the real citations in a run. Both
//! `regex-match-in-glob` and `regex-must-not-match` share one implementation:
//! the collector reports only *whether the pattern was found and where*, and
//! the pack's outcome policy decides what that means. Inverting the sense is
//! the pack author's job, not the collector's.

use regex::Regex;
use sc_proto::{DcError, Result};

use crate::collector::{AuditContext, Collector, Observation};
use crate::evidence::Evidence;
use crate::glob::Glob;
use crate::pack::{Check, CheckKind};

/// The compliance tool's own sources, never searched by a text check.
///
/// A secret detector matches its own detection pattern; a weak-crypto check
/// matches the regex that defines it. Excluding these structurally — rather than
/// asking every pack author to remember `exclude_globs` — is what keeps the
/// collection scalable: without it, each new pattern added to any pack becomes a
/// potential finding for every other pack.
///
/// Deliberately narrow: only this crate and its authoring sibling. A project that
/// legitimately vendors compliance tooling still gets scanned.
pub const TOOL_SOURCE_GLOBS: &[&str] = &[
    "crates/sc-comply/src/**/*",
    "crates/sc-comply/packs/**/*.toml",
    "crates/sc-comply-author/src/**/*",
    "crates/sc-comply-author/catalogs/*.toml",
    "crates/sc-comply-author/evals/*.toml",
];

/// Handles `regex-match-in-glob` and `regex-must-not-match`.
pub struct RegexCollector;

impl Collector for RegexCollector {
    fn name(&self) -> &'static str {
        "regex"
    }

    fn handles(&self, kind: &CheckKind) -> bool {
        matches!(
            kind,
            CheckKind::RegexMatchInGlob { .. } | CheckKind::RegexMustNotMatch { .. }
        )
    }

    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation> {
        let (glob_src, pattern) = match &check.kind {
            CheckKind::RegexMatchInGlob { glob, pattern }
            | CheckKind::RegexMustNotMatch { glob, pattern } => (glob, pattern),
            other => {
                return Err(DcError::Comply(format!(
                    "RegexCollector cannot handle {}",
                    other.label()
                )))
            }
        };

        // Both already compiled once at pack load; recompiling here keeps the
        // pack types plain-`Deserialize` and the cost is negligible against
        // the file scan.
        let glob = Glob::new(glob_src)?;
        let re = Regex::new(pattern)
            .map_err(|e| DcError::Comply(format!("check {:?}: invalid regex: {e}", check.id)))?;

        let mut excludes: Vec<Glob> = check
            .exclude_globs
            .iter()
            .map(|g| Glob::new(g))
            .collect::<Result<Vec<_>>>()?;

        // The tool's own sources are ALWAYS excluded, without the pack having to
        // say so. A detector inevitably matches its own detection patterns, and
        // requiring every author to remember `exclude_globs` scales badly: each
        // new pattern lands in the packs directory and becomes a potential hit
        // for every other pack, so adding a control to PCI could break the ISO
        // audit. Making it structural removes the whole class of failure.
        for g in TOOL_SOURCE_GLOBS {
            excludes.push(Glob::new(g)?);
        }

        let matched_glob: Vec<_> = ctx
            .files
            .iter()
            .filter(|f| glob.is_match(&f.path))
            .collect();

        let selected: Vec<_> = matched_glob
            .iter()
            .filter(|f| !excludes.iter().any(|e| e.is_match(&f.path)))
            // `tracked_only` drops gitignored files. A control about what was
            // COMMITTED must not fire on a file that is not in the repository —
            // saying "committed to source" of an untracked file is simply false.
            .filter(|f| !(check.tracked_only && f.ignored))
            .copied()
            .collect();

        // Suppression is a correctness hazard, so it is never silent: if an
        // exclusion actually removed a file from consideration, the report says
        // so and names how many. Built-in tool-source exclusions are named
        // separately from the pack's own, so a reader is not told the author
        // suppressed something they did not.
        let excluded_count = matched_glob.len() - selected.len();
        let exclusion_note = (excluded_count > 0).then(|| {
            if check.exclude_globs.is_empty() {
                format!("{excluded_count} file(s) excluded (the tool's own sources)")
            } else {
                format!(
                    "{excluded_count} file(s) excluded by {:?} and the tool's own sources",
                    check.exclude_globs
                )
            }
        });

        if selected.is_empty() {
            // The distinction the whole lattice exists for: we did not look at
            // anything, which is not the same as looking and finding nothing.
            let why = if excluded_count > 0 {
                format!(
                    "no files left to search for glob {glob_src:?} after {excluded_count} \
                     exclusion(s)"
                )
            } else {
                format!("no files matched glob {glob_src:?}")
            };
            return Ok(Observation::indeterminate(why));
        }

        let cap = ctx.options.max_evidence_per_check;
        let mut evidence = Vec::new();
        let mut total_hits = 0usize;

        for f in &selected {
            for (lineno, line) in f.lines() {
                if re.is_match(line) {
                    total_hits += 1;
                    if evidence.len() < cap {
                        evidence.push(
                            Evidence::new(&f.path, Some(lineno), line, &check.id, self.name())
                                .untracked(f.ignored),
                        );
                    }
                }
            }
        }

        if total_hits == 0 {
            return Ok(Observation {
                matched: Some(false),
                evidence: vec![],
                note: Some(join_notes(
                    format!(
                        "no match in {} file(s) matching {glob_src:?}",
                        selected.len()
                    ),
                    exclusion_note,
                )),
            });
        }

        // Never silently truncate: a report that shows 20 of 500 hits while
        // implying it showed all of them is worse than one that says so.
        let truncation_note = (total_hits > evidence.len()).then(|| {
            format!(
                "{total_hits} hit(s) across {} file(s); showing the first {}",
                selected.len(),
                evidence.len()
            )
        });

        let note = match (truncation_note, exclusion_note) {
            (Some(t), Some(x)) => Some(format!("{t}; {x}")),
            (Some(t), None) => Some(t),
            (None, Some(x)) => Some(x),
            (None, None) => None,
        };

        Ok(Observation {
            matched: Some(true),
            evidence,
            note,
        })
    }
}

/// Append an optional secondary note to a primary one.
fn join_notes(primary: String, extra: Option<String>) -> String {
    match extra {
        Some(x) => format!("{primary}; {x}"),
        None => primary,
    }
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

    fn check(id: &str, kind: CheckKind) -> Check {
        Check {
            id: id.to_string(),
            kind,
            on_match: Outcome::Pass,
            on_no_match: Outcome::Gap,
            on_no_files: None,
            weight: 1.0,
            exclude_globs: vec![],
            tracked_only: false,
            rationale: String::new(),
        }
    }

    fn check_excluding(id: &str, kind: CheckKind, excludes: &[&str]) -> Check {
        Check {
            exclude_globs: excludes.iter().map(|s| s.to_string()).collect(),
            ..check(id, kind)
        }
    }

    fn ignored_file(path: &str, contents: &str) -> TextFile {
        TextFile {
            path: path.to_string(),
            contents: contents.to_string(),
            ignored: true,
        }
    }

    #[test]
    fn tracked_only_skips_gitignored_files() {
        // The fix for the CC6.1 overclaim: a control titled "not committed to
        // source" must not fire on a gitignored file, because that file is not
        // in the repository and the claim would be false.
        let files = vec![
            file("src/lib.rs", "fn main() {}\n"),
            ignored_file("local.key", "-----BEGIN EC PRIVATE KEY-----\n"),
        ];
        let c = Check {
            tracked_only: true,
            ..check(
                "committed-keys",
                CheckKind::RegexMustNotMatch {
                    glob: "**/*".into(),
                    pattern: "BEGIN EC PRIVATE KEY".into(),
                },
            )
        };
        let o = run(&files, &c);
        assert_eq!(
            o.matched,
            Some(false),
            "an untracked key is not committed to source"
        );
    }

    #[test]
    fn without_tracked_only_the_same_file_is_still_seen() {
        // The companion control depends on this: on-disk exposure is real and
        // must still be reportable, just under an honestly-worded control.
        let files = vec![ignored_file(
            "local.key",
            "-----BEGIN EC PRIVATE KEY-----\n",
        )];
        let c = check(
            "on-disk-keys",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "BEGIN EC PRIVATE KEY".into(),
            },
        );
        let o = run(&files, &c);
        assert_eq!(o.matched, Some(true));
        assert!(o.evidence[0].untracked, "and it is labelled untracked");
    }

    #[test]
    fn tracked_only_still_finds_a_genuinely_committed_secret() {
        // The fix must not blunt the control it protects.
        let files = vec![
            file("committed.key", "-----BEGIN EC PRIVATE KEY-----\n"),
            ignored_file("local.key", "-----BEGIN EC PRIVATE KEY-----\n"),
        ];
        let c = Check {
            tracked_only: true,
            ..check(
                "committed-keys",
                CheckKind::RegexMustNotMatch {
                    glob: "**/*".into(),
                    pattern: "BEGIN EC PRIVATE KEY".into(),
                },
            )
        };
        let o = run(&files, &c);
        assert_eq!(o.matched, Some(true), "a tracked key is still a finding");
        assert_eq!(o.evidence.len(), 1);
        assert_eq!(o.evidence[0].file, "committed.key");
    }

    fn run_with(files: &[TextFile], c: &Check, opts: &ComplyOptions) -> Observation {
        let root = Path::new("/ws");
        let ctx = AuditContext::new(root, files, opts);
        RegexCollector.collect(c, &ctx).expect("collect")
    }

    fn run(files: &[TextFile], c: &Check) -> Observation {
        run_with(files, c, &ComplyOptions::default())
    }

    #[test]
    fn finds_a_planted_private_key_with_exact_line_and_excerpt() {
        let files = vec![
            file("src/lib.rs", "fn main() {}\n"),
            file(
                "deploy/id_rsa",
                "header\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n",
            ),
        ];
        let c = check(
            "no-keys",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "-----BEGIN (RSA )?PRIVATE KEY-----".into(),
            },
        );
        let o = run(&files, &c);

        assert_eq!(o.matched, Some(true), "the key must be found");
        assert_eq!(o.evidence.len(), 1);
        let e = &o.evidence[0];
        assert_eq!(e.file, "deploy/id_rsa");
        assert_eq!(e.line, Some(2), "line numbers are 1-based");
        assert_eq!(e.excerpt, "-----BEGIN RSA PRIVATE KEY-----");
        assert_eq!(e.check_id, "no-keys");
        assert_eq!(e.produced_by, "regex");
    }

    #[test]
    fn a_glob_matching_zero_files_is_indeterminate_not_a_pass() {
        // The single most important behaviour in this collector.
        let files = vec![file("src/lib.rs", "fn main() {}\n")];
        let c = check(
            "tls",
            CheckKind::RegexMatchInGlob {
                glob: "**/*.tf".into(),
                pattern: "min_tls_version".into(),
            },
        );
        let o = run(&files, &c);

        assert_eq!(o.matched, None);
        let note = o.note.expect("indeterminate must explain itself");
        assert!(note.contains("no files matched"), "{note}");
    }

    #[test]
    fn no_hit_in_matched_files_is_a_definite_negative() {
        // We looked and it genuinely was not there — distinct from the above.
        let files = vec![file("main.tf", "resource \"aws_s3_bucket\" \"b\" {}\n")];
        let c = check(
            "tls",
            CheckKind::RegexMatchInGlob {
                glob: "**/*.tf".into(),
                pattern: "min_tls_version".into(),
            },
        );
        let o = run(&files, &c);

        assert_eq!(o.matched, Some(false));
        assert!(o.note.expect("note").contains("no match in 1 file"));
    }

    #[test]
    fn glob_restricts_which_files_are_searched() {
        let files = vec![
            file("src/a.rs", "let password = \"hunter2\";\n"),
            file("notes.txt", "let password = \"hunter2\";\n"),
        ];
        let c = check(
            "pw",
            CheckKind::RegexMatchInGlob {
                glob: "**/*.rs".into(),
                pattern: "password".into(),
            },
        );
        let o = run(&files, &c);
        assert_eq!(o.evidence.len(), 1);
        assert_eq!(o.evidence[0].file, "src/a.rs");
    }

    #[test]
    fn evidence_is_capped_and_the_truncation_is_stated() {
        let body: String = (0..50).map(|i| format!("hit {i}\n")).collect();
        let files = vec![file("big.txt", &body)];
        let opts = ComplyOptions {
            max_evidence_per_check: 5,
            ..Default::default()
        };
        let c = check(
            "hits",
            CheckKind::RegexMatchInGlob {
                glob: "**/*".into(),
                pattern: "hit".into(),
            },
        );
        let o = run_with(&files, &c, &opts);

        assert_eq!(o.matched, Some(true));
        assert_eq!(o.evidence.len(), 5);
        let note = o.note.expect("truncation must be stated, never silent");
        assert!(note.contains("50 hit(s)"), "{note}");
        assert!(note.contains("first 5"), "{note}");
    }

    #[test]
    fn no_truncation_note_when_everything_fits() {
        let files = vec![file("a.txt", "hit\n")];
        let o = run(
            &files,
            &check(
                "hits",
                CheckKind::RegexMatchInGlob {
                    glob: "**/*".into(),
                    pattern: "hit".into(),
                },
            ),
        );
        assert_eq!(o.note, None);
    }

    #[test]
    fn hits_across_multiple_files_are_all_cited() {
        let files = vec![
            file("a.rs", "danger_accept_invalid_certs(true)\n"),
            file("b.rs", "ok\ndanger_accept_invalid_certs(true)\n"),
        ];
        let c = check(
            "tls-off",
            CheckKind::RegexMustNotMatch {
                glob: "**/*.rs".into(),
                pattern: "danger_accept_invalid_certs\\s*\\(\\s*true".into(),
            },
        );
        let o = run(&files, &c);
        assert_eq!(o.evidence.len(), 2);
        assert_eq!(o.evidence[0].locator(), "a.rs:1");
        assert_eq!(o.evidence[1].locator(), "b.rs:2");
    }

    #[test]
    fn case_insensitive_patterns_work() {
        let files = vec![file("a.yml", "SSL_Protocols TLSv1.2\n")];
        let c = check(
            "tls",
            CheckKind::RegexMatchInGlob {
                glob: "**/*.yml".into(),
                pattern: "(?i)ssl_protocols\\s+TLSv1\\.[23]".into(),
            },
        );
        assert_eq!(run(&files, &c).matched, Some(true));
    }

    #[test]
    fn exclude_globs_suppress_matching_files() {
        // A detector inevitably matches its own detection pattern. Without
        // this, the tool reports its own test fixtures as findings and buries
        // the real one.
        let files = vec![
            file("src/collectors/text.rs", "assert!(\"SECRET_MARKER\");\n"),
            file("deploy/real.key", "SECRET_MARKER\n"),
        ];
        let c = check_excluding(
            "marker",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "SECRET_MARKER".into(),
            },
            &["src/collectors/*"],
        );
        let o = run(&files, &c);

        assert_eq!(o.matched, Some(true));
        assert_eq!(o.evidence.len(), 1, "only the real finding should remain");
        assert_eq!(o.evidence[0].file, "deploy/real.key");
    }

    #[test]
    fn the_tools_own_sources_are_excluded_without_the_pack_asking() {
        // The scaling fix: a pack author must NOT have to remember
        // exclude_globs for the detector's own sources. Without this, every new
        // pattern added to any pack becomes a potential finding for every other
        // pack — adding a control to PCI could break the ISO audit.
        let files = vec![
            file(
                "crates/sc-comply/packs/soc2-tsc/code.toml",
                "pattern = \"BEGIN RSA PRIVATE KEY\"\n",
            ),
            file(
                "crates/sc-comply/src/collectors/text.rs",
                "\"BEGIN RSA PRIVATE KEY\"\n",
            ),
            file("deploy/real.key", "-----BEGIN RSA PRIVATE KEY-----\n"),
        ];
        // NOTE: no exclude_globs at all on this check.
        let c = check(
            "keys",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "BEGIN RSA PRIVATE KEY".into(),
            },
        );
        let o = run(&files, &c);

        assert_eq!(o.matched, Some(true));
        assert_eq!(
            o.evidence.len(),
            1,
            "only the real finding: {:?}",
            o.evidence
        );
        assert_eq!(o.evidence[0].file, "deploy/real.key");
    }

    #[test]
    fn the_builtin_exclusion_is_disclosed_distinctly() {
        // Suppression is never silent, but a reader must not be told the pack
        // author suppressed something the tool suppressed for them.
        let files = vec![
            file("crates/sc-comply/src/x.rs", "SECRET_MARKER\n"),
            file("real.txt", "SECRET_MARKER\n"),
        ];
        let c = check(
            "marker",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "SECRET_MARKER".into(),
            },
        );
        let note = run(&files, &c).note.expect("disclosed");
        assert!(note.contains("the tool's own sources"), "{note}");
        // No pack-declared globs, so it must not claim any.
        assert!(!note.contains("[]"), "{note}");
    }

    #[test]
    fn a_project_that_vendors_the_tool_still_scans_its_own_code() {
        // The exclusion is deliberately narrow — it must not blind the scanner
        // to an audited project's real sources.
        let files = vec![file("src/collectors/text.rs", "SECRET_MARKER\n")];
        let c = check(
            "marker",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "SECRET_MARKER".into(),
            },
        );
        assert_eq!(run(&files, &c).matched, Some(true));
    }

    #[test]
    fn exclusions_are_disclosed_never_silent() {
        // Suppression can hide genuine findings, so the report must say that
        // files were withheld and how many.
        let files = vec![
            file("fixtures/a.txt", "SECRET_MARKER\n"),
            file("real.txt", "SECRET_MARKER\n"),
        ];
        let c = check_excluding(
            "marker",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "SECRET_MARKER".into(),
            },
            &["fixtures/**/*"],
        );
        let note = run(&files, &c).note.expect("exclusions must be disclosed");
        assert!(note.contains("1 file(s) excluded"), "{note}");
        assert!(note.contains("fixtures/**/*"), "{note}");
    }

    #[test]
    fn excluding_every_candidate_is_indeterminate_not_a_pass() {
        // Critical: an over-broad exclusion must not read as a clean result.
        // If we excluded everything we could have looked at, we did not look.
        let files = vec![file("fixtures/a.txt", "SECRET_MARKER\n")];
        let c = check_excluding(
            "marker",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "SECRET_MARKER".into(),
            },
            &["**/*"],
        );
        let o = run(&files, &c);

        assert_eq!(o.matched, None, "excluding everything is not a pass");
        assert!(o.note.expect("note").contains("exclusion"));
    }

    #[test]
    fn no_exclusion_note_when_nothing_was_suppressed() {
        let files = vec![file("real.txt", "SECRET_MARKER\n")];
        let c = check_excluding(
            "marker",
            CheckKind::RegexMustNotMatch {
                glob: "**/*".into(),
                pattern: "SECRET_MARKER".into(),
            },
            &["nonexistent/**/*"],
        );
        assert_eq!(run(&files, &c).note, None);
    }

    #[test]
    fn rejects_a_kind_it_does_not_handle() {
        let c = check(
            "x",
            CheckKind::FileAbsent {
                path: ".env".into(),
            },
        );
        assert!(!RegexCollector.handles(&c.kind));
        let ctx_files: Vec<TextFile> = vec![];
        let opts = ComplyOptions::default();
        let ctx = AuditContext::new(Path::new("/ws"), &ctx_files, &opts);
        assert!(RegexCollector.collect(&c, &ctx).is_err());
    }
}
