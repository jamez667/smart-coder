//! Presence and absence of named artifacts.

use sc_proto::{DcError, Result};

use crate::collector::{AuditContext, Collector, Observation};
use crate::evidence::Evidence;
use crate::pack::{Check, CheckKind};
use crate::scan::path_exists;

/// Handles `file-exists` and `file-absent`.
pub struct FileCollector;

impl Collector for FileCollector {
    fn name(&self) -> &'static str {
        "file"
    }

    fn handles(&self, kind: &CheckKind) -> bool {
        matches!(
            kind,
            CheckKind::FileExists { .. } | CheckKind::FileAbsent { .. }
        )
    }

    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation> {
        match &check.kind {
            CheckKind::FileExists { paths } => {
                // Any-of: the first hit is the evidence.
                for p in paths {
                    if path_exists(ctx.root, p) {
                        return Ok(Observation::matched(vec![Evidence::new(
                            p,
                            None,
                            format!("present: {p}"),
                            &check.id,
                            self.name(),
                        )]));
                    }
                }
                // Absence has no citable artifact, so the evidence records what
                // we looked for. An auditor needs to see the search, not just
                // the verdict.
                Ok(Observation {
                    matched: Some(false),
                    evidence: vec![],
                    note: Some(format!("none of these exist: {}", paths.join(", "))),
                })
            }

            CheckKind::FileAbsent { path } => {
                if path_exists(ctx.root, path) {
                    let ignored = ctx.files.iter().any(|f| f.path == *path && f.ignored)
                        || crate::scan::IgnoreRules::load(ctx.root).is_ignored(path);

                    // `tracked_only`: a gitignored file is not in the repository,
                    // so for a control about what was COMMITTED it is simply not
                    // a finding. Reporting it as one would state something false.
                    if check.tracked_only && ignored {
                        return Ok(Observation {
                            matched: Some(false),
                            evidence: vec![],
                            note: Some(format!(
                                "{path} exists locally but is gitignored, so it is not committed to \
                                 source; this check only considers tracked files"
                            )),
                        });
                    }

                    // Otherwise: "matched" means the file WAS found, which for
                    // this kind is the bad outcome. A gitignored hit is labelled
                    // rather than dropped — an untracked `.env` is a real local
                    // exposure, just not a committed one.
                    Ok(Observation::matched(vec![Evidence::new(
                        path,
                        None,
                        format!("present: {path}"),
                        &check.id,
                        self.name(),
                    )
                    .untracked(ignored)]))
                } else {
                    Ok(Observation::not_matched(vec![]))
                }
            }

            other => Err(DcError::Comply(format!(
                "FileCollector cannot handle {}",
                other.label()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::ComplyOptions;
    use crate::status::Outcome;
    use crate::test_support::{temp_repo, write};
    use std::path::Path;

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

    fn run(root: &Path, c: &Check) -> Observation {
        let opts = ComplyOptions::default();
        let files = vec![];
        let ctx = AuditContext::new(root, &files, &opts);
        FileCollector.collect(c, &ctx).expect("collect")
    }

    #[test]
    fn file_exists_finds_the_first_present_path() {
        let root = temp_repo("file-exists");
        write(&root, "CONTRIBUTING.md", "how to contribute\n");

        let c = check(
            "contrib",
            CheckKind::FileExists {
                paths: vec!["docs/CONTRIBUTING.md".into(), "CONTRIBUTING.md".into()],
            },
        );
        let o = run(&root, &c);
        assert_eq!(o.matched, Some(true));
        assert_eq!(o.evidence.len(), 1);
        assert_eq!(o.evidence[0].file, "CONTRIBUTING.md");
        assert_eq!(o.evidence[0].produced_by, "file");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_exists_accepts_a_directory() {
        // `.github/workflows` is a directory. Treating it as absent would emit
        // a false CC8.1 gap on every GitHub repository.
        let root = temp_repo("file-exists-dir");
        write(&root, ".github/workflows/ci.yml", "on: push\n");

        let c = check(
            "ci",
            CheckKind::FileExists {
                paths: vec![".github/workflows".into()],
            },
        );
        let o = run(&root, &c);
        assert_eq!(o.matched, Some(true), "a directory must count as present");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_exists_reports_what_it_looked_for_when_absent() {
        let root = temp_repo("file-exists-missing");
        let c = check(
            "ci",
            CheckKind::FileExists {
                paths: vec!["Jenkinsfile".into(), ".circleci".into()],
            },
        );
        let o = run(&root, &c);
        assert_eq!(o.matched, Some(false));
        let note = o.note.expect("note");
        assert!(note.contains("Jenkinsfile"), "{note}");
        assert!(note.contains(".circleci"), "{note}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_absent_inverts_the_match_sense() {
        let root = temp_repo("file-absent");
        write(&root, ".env", "SECRET=hunter2\n");

        let present = check(
            "env",
            CheckKind::FileAbsent {
                path: ".env".into(),
            },
        );
        let o = run(&root, &present);
        // A "match" for file-absent means the file exists, which the pack maps
        // to a gap.
        assert_eq!(o.matched, Some(true));
        assert_eq!(o.evidence[0].file, ".env");

        let missing = check(
            "other",
            CheckKind::FileAbsent {
                path: "nope.env".into(),
            },
        );
        let o2 = run(&root, &missing);
        assert_eq!(o2.matched, Some(false));
        assert!(o2.evidence.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tracked_only_does_not_flag_a_gitignored_file() {
        // A gitignored .env is CORRECT practice. Flagging it makes the check
        // fire on every well-configured repository, which is how a scanner
        // trains its readers to ignore it.
        let root = temp_repo("file-absent-ignored");
        write(&root, ".gitignore", ".env\n");
        write(&root, ".env", "SECRET=hunter2\n");

        let c = Check {
            tracked_only: true,
            ..check(
                "env-not-tracked",
                CheckKind::FileAbsent {
                    path: ".env".into(),
                },
            )
        };
        let o = run(&root, &c);
        assert_eq!(o.matched, Some(false), "a gitignored .env is not committed");
        assert!(o.evidence.is_empty());
        let note = o.note.expect("must explain why it was not a finding");
        assert!(note.contains("gitignored"), "{note}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tracked_only_still_flags_a_tracked_file() {
        // The fix must not blunt the control: a .env that is NOT gitignored is
        // a genuine finding.
        let root = temp_repo("file-absent-tracked");
        write(&root, ".gitignore", "target/\n");
        write(&root, ".env", "SECRET=hunter2\n");

        let c = Check {
            tracked_only: true,
            ..check(
                "env-not-tracked",
                CheckKind::FileAbsent {
                    path: ".env".into(),
                },
            )
        };
        assert_eq!(run(&root, &c).matched, Some(true));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_a_kind_it_does_not_handle() {
        let root = temp_repo("file-wrongkind");
        let c = check(
            "x",
            CheckKind::RegexMatchInGlob {
                glob: "**/*".into(),
                pattern: "x".into(),
            },
        );
        assert!(!FileCollector.handles(&c.kind));

        let opts = ComplyOptions::default();
        let files = vec![];
        let ctx = AuditContext::new(&root, &files, &opts);
        assert!(FileCollector.collect(&c, &ctx).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
