//! Symbol-name evidence, via `sc-index`'s tree-sitter extraction.
//!
//! This is the one collector where `sc-index`'s language filter is a feature
//! rather than an obstacle: we genuinely want parsed definitions, not text
//! matches, so that a symbol named in a comment or a string literal does not
//! count as evidence that it exists.
//!
//! The cost is a hard limit: Rust, Python and C# only. A workspace with no
//! indexable source at all must report `Unknown` and say why — reporting "no
//! such symbol" for a Go codebase would be a false negative dressed as a
//! finding.

use regex::Regex;
use sc_index::{extract_symbols, Language};
use sc_proto::{DcError, Result};

use crate::collector::{AuditContext, Collector, Observation};
use crate::evidence::Evidence;
use crate::pack::{Check, CheckKind, LangSel};

/// Handles `symbol-exists`.
pub struct SymbolCollector;

impl LangSel {
    fn to_index_language(self) -> Language {
        match self {
            LangSel::Rust => Language::Rust,
            LangSel::Python => Language::Python,
            LangSel::CSharp => Language::CSharp,
        }
    }
}

impl Collector for SymbolCollector {
    fn name(&self) -> &'static str {
        "symbol"
    }

    fn handles(&self, kind: &CheckKind) -> bool {
        matches!(kind, CheckKind::SymbolExists { .. })
    }

    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation> {
        let (name_pattern, languages) = match &check.kind {
            CheckKind::SymbolExists {
                name_pattern,
                languages,
            } => (name_pattern, languages),
            other => {
                return Err(DcError::Comply(format!(
                    "SymbolCollector cannot handle {}",
                    other.label()
                )))
            }
        };

        let re = Regex::new(name_pattern).map_err(|e| {
            DcError::Comply(format!("check {:?}: invalid symbol regex: {e}", check.id))
        })?;

        // An empty `languages` list means "every language sc-index supports".
        let wanted: Vec<Language> = if languages.is_empty() {
            vec![Language::Rust, Language::Python, Language::CSharp]
        } else {
            languages.iter().map(|l| l.to_index_language()).collect()
        };

        // Reuse the already-scanned file contents rather than re-walking; only
        // the language filter is applied here.
        let indexable: Vec<(&str, Language, &str)> = ctx
            .files
            .iter()
            .filter_map(|f| {
                let lang = Language::from_path(&f.path)?;
                wanted
                    .contains(&lang)
                    .then_some((f.path.as_str(), lang, f.contents.as_str()))
            })
            .collect();

        if indexable.is_empty() {
            return Ok(Observation::indeterminate(format!(
                "no indexable source files for this check; sc-index supports Rust, Python and C# only \
                 (looked for languages: {})",
                describe_languages(&wanted)
            )));
        }

        let cap = ctx.options.max_evidence_per_check;
        let mut evidence = Vec::new();
        let mut total = 0usize;

        for (path, lang, source) in &indexable {
            let syms = extract_symbols(*lang, source);
            for def in &syms.defs {
                if re.is_match(&def.name) {
                    total += 1;
                    if evidence.len() < cap {
                        evidence.push(Evidence::new(
                            *path,
                            Some(def.line as u32),
                            &def.name,
                            &check.id,
                            self.name(),
                        ));
                    }
                }
            }
        }

        if total == 0 {
            return Ok(Observation {
                matched: Some(false),
                evidence: vec![],
                note: Some(format!(
                    "no matching symbol in {} indexable file(s)",
                    indexable.len()
                )),
            });
        }

        let note = (total > evidence.len()).then(|| {
            format!(
                "{total} matching symbol(s); showing the first {}",
                evidence.len()
            )
        });

        Ok(Observation {
            matched: Some(true),
            evidence,
            note,
        })
    }
}

fn describe_languages(langs: &[Language]) -> String {
    langs
        .iter()
        .map(|l| match l {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::CSharp => "csharp",
        })
        .collect::<Vec<_>>()
        .join(", ")
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

    fn check(id: &str, pattern: &str, languages: Vec<LangSel>) -> Check {
        Check {
            id: id.to_string(),
            kind: CheckKind::SymbolExists {
                name_pattern: pattern.to_string(),
                languages,
            },
            on_match: Outcome::Pass,
            on_no_match: Outcome::Unknown,
            on_no_files: None,
            weight: 1.0,
            exclude_globs: vec![],
            tracked_only: false,
            rationale: String::new(),
        }
    }

    fn run(files: &[TextFile], c: &Check) -> Observation {
        let opts = ComplyOptions::default();
        let ctx = AuditContext::new(Path::new("/ws"), files, &opts);
        SymbolCollector.collect(c, &ctx).expect("collect")
    }

    #[test]
    fn finds_a_rust_function_definition() {
        let files = vec![file(
            "src/tel.rs",
            "fn unrelated() {}\n\nfn init_tracing() {\n    // set up\n}\n",
        )];
        let c = check("tel", "(?i)init_tracing", vec![LangSel::Rust]);
        let o = run(&files, &c);

        assert_eq!(o.matched, Some(true));
        assert_eq!(o.evidence.len(), 1);
        assert_eq!(o.evidence[0].file, "src/tel.rs");
        assert_eq!(o.evidence[0].excerpt, "init_tracing");
        assert_eq!(o.evidence[0].produced_by, "symbol");
    }

    #[test]
    fn finds_a_python_definition() {
        let files = vec![file("app/log.py", "def setup_logging():\n    pass\n")];
        let c = check("log", "setup_logging", vec![LangSel::Python]);
        assert_eq!(run(&files, &c).matched, Some(true));
    }

    #[test]
    fn a_workspace_with_no_indexable_files_is_unknown_with_a_reason() {
        // The important negative: a Go or YAML-only repo must not be reported
        // as "symbol not found". Mirrors sc-index's own
        // find_symbol_explains_when_no_indexable_files.
        let files = vec![
            file("ci.yml", "on: push\n"),
            file("main.go", "func initTracing() {}\n"),
        ];
        let c = check("tel", "init_tracing", vec![]);
        let o = run(&files, &c);

        assert_eq!(o.matched, None);
        let note = o.note.expect("must explain the language limitation");
        assert!(note.contains("Rust, Python and C#"), "{note}");
    }

    #[test]
    fn language_filter_excludes_other_languages() {
        // The symbol exists, but in a language the check did not ask for.
        let files = vec![
            file("app/log.py", "def setup_logging():\n    pass\n"),
            file("src/a.rs", "fn other() {}\n"),
        ];
        let c = check("log", "setup_logging", vec![LangSel::Rust]);
        let o = run(&files, &c);
        assert_eq!(
            o.matched,
            Some(false),
            "rust files existed, so we could look"
        );
    }

    #[test]
    fn language_filter_yields_unknown_when_no_file_of_that_language_exists() {
        let files = vec![file("app/log.py", "def setup_logging():\n    pass\n")];
        let c = check("log", "setup_logging", vec![LangSel::Rust]);
        let o = run(&files, &c);
        assert_eq!(
            o.matched, None,
            "no rust files at all means we could not look"
        );
    }

    #[test]
    fn a_name_in_a_comment_is_not_a_definition() {
        // The reason this collector uses tree-sitter rather than a regex.
        let files = vec![file(
            "src/a.rs",
            "// TODO: add init_tracing here\nfn other() {}\n",
        )];
        let c = check("tel", "init_tracing", vec![LangSel::Rust]);
        assert_eq!(run(&files, &c).matched, Some(false));
    }

    #[test]
    fn no_matching_symbol_reports_how_many_files_were_searched() {
        let files = vec![file("src/a.rs", "fn other() {}\n")];
        let c = check("tel", "init_tracing", vec![LangSel::Rust]);
        let o = run(&files, &c);
        assert_eq!(o.matched, Some(false));
        assert!(o.note.expect("note").contains("1 indexable file"));
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
            tracked_only: false,
            rationale: String::new(),
        };
        assert!(!SymbolCollector.handles(&c.kind));
        let files: Vec<TextFile> = vec![];
        let opts = ComplyOptions::default();
        let ctx = AuditContext::new(Path::new("/ws"), &files, &opts);
        assert!(SymbolCollector.collect(&c, &ctx).is_err());
    }
}
