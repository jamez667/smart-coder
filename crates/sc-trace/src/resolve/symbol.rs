//! Symbol anchors: `<!--@ sc_workflow::Phase::ALL len=5 -->`.
//!
//! The whole rule rests on one asymmetry:
//!
//! > **The crate segment is reliable. The module segments are not.**
//!
//! The crate segment maps to a workspace member, verifiably. Module segments do
//! not map to anything checkable, because Rust re-exports mean a symbol's
//! *use-path* routinely differs from its *file path* — `sc_workflow::artifact_dirs`
//! is a real anchor in spec 19, and it lives in `artifact_dir.rs`. A rule that
//! required module segments to match directories would report `Broken` there.
//!
//! That matters more than it sounds. **A false `Broken` is how this gate gets
//! deleted.** A checker that cries wolf on correct documentation teaches people
//! to bypass it, and a bypassed gate protects nothing. So the rule is: reject on
//! the crate segment (unambiguous), and everywhere else prefer `Unknown` — which
//! is honest, visible, and does not fail a build.

use std::path::Path;

use sc_index::{extract_symbols, Language, SourceFile};

use super::cardinality::{self, LenVerdict};
use super::{Located, Resolution};
use crate::anchor::SymbolRef;
use crate::manifest::{Crate, Workspace};

/// One candidate definition of the anchored name.
struct Candidate {
    path: String,
    line: usize,
    source: String,
}

/// Resolve a symbol anchor, optionally checking a `len=` assertion.
pub fn resolve(sym: &SymbolRef, root: &Path, ws: &Workspace, expect: Option<usize>) -> Resolution {
    // 1. The crate segment must name a workspace member. This is the ONE place a
    //    symbol anchor may be rejected outright, because it is unambiguous: the
    //    manifest is ground truth about what crates exist.
    let Some(krate) = ws.by_lib_name(&sym.crate_seg) else {
        return Resolution::broken(format!(
            "no crate `{}` in this workspace (anchor: {})",
            sym.crate_seg,
            sym.display_path()
        ));
    };

    // 2. Search only that crate. Scoping is what makes common names tractable —
    //    `main`, `run` and `parse` collide many ways workspace-wide.
    let sources = crate_sources(root, krate);
    if sources.is_empty() {
        // A crate with no indexable Rust is a limit of the checker, not a
        // missing symbol. Reporting Broken here would be a lie about the code.
        return Resolution::unknown(format!(
            "crate `{}` has no indexable Rust files, so `{}` cannot be located",
            krate.name, sym.name
        ));
    }

    let mut candidates = find_candidates(&sources, &sym.name);

    // 3. An owner (`Phase::ALL`) narrows to associated items of that type. Five
    //    distinct `ALL` consts exist in this workspace, so without this they are
    //    indistinguishable.
    if let Some(owner) = &sym.owner {
        candidates.retain(|c| cardinality::measure(&c.source, &sym.name, Some(owner)).is_some());
    }

    match candidates.len() {
        // 4. Nothing here. The crate exists and we parsed all of it, so this is
        //    a real Broken — enriched with where it DID turn up, which makes a
        //    moved symbol immediately actionable.
        0 => Resolution::broken(missing_note(sym, krate, root, ws)),
        1 => {
            let hit = &candidates[0];
            let located = Located::at(&hit.path, hit.line);
            match expect {
                None => Resolution::ok(vec![located]),
                Some(expect) => check_len(sym, hit, located, expect),
            }
        }
        // 5. Several definitions share the name in one crate. Ambiguity is a
        //    limit of the checker, NOT a spec error — never Broken.
        _ => {
            let targets: Vec<Located> = candidates
                .iter()
                .map(|c| Located::at(&c.path, c.line))
                .collect();
            let where_ = targets
                .iter()
                .map(|t| t.display())
                .collect::<Vec<_>>()
                .join(", ");
            Resolution::unknown_at(
                targets,
                format!(
                    "`{}` is ambiguous in crate `{}` ({} definitions: {where_}) — \
                     name the owning type to disambiguate",
                    sym.name,
                    krate.name,
                    candidates.len()
                ),
            )
        }
    }
}

/// Check a `len=N` assertion against one located definition.
fn check_len(sym: &SymbolRef, hit: &Candidate, located: Located, expect: usize) -> Resolution {
    let Some(card) = cardinality::measure(&hit.source, &sym.name, sym.owner.as_deref()) else {
        // The def index found it but the measurement pass did not — different
        // parsers, so say so rather than inventing a verdict.
        return Resolution::unknown_at(
            vec![located],
            format!("`{}` could not be measured for `len=`", sym.name),
        );
    };
    match cardinality::verdict(&card, expect) {
        LenVerdict::Ok => Resolution::ok(vec![located]),
        LenVerdict::Stale { detail } => Resolution::stale(vec![located], detail),
        // The code contradicts itself — a dead array slot. Stale against the
        // code rather than the spec, and reported whatever N the spec claimed.
        LenVerdict::Inconsistent { elements, declared } => Resolution::stale(
            vec![located],
            format!(
                "the code is inconsistent: {elements} elements declared as length \
                 {declared} (a dead slot). Spec says len={expect}"
            ),
        ),
        LenVerdict::Unmeasurable { why } => Resolution::unknown_at(
            vec![located],
            format!("`{}` resolved but len= is unmeasurable: {why}", sym.name),
        ),
    }
}

/// A `Broken` note that says where the symbol actually is, if anywhere.
///
/// The workspace-wide search here **only enriches the message** and never changes
/// the verdict: a symbol that moved crates is still a broken anchor, but "not
/// here — it is in `sc_core`" is immediately actionable where "not found" is not.
fn missing_note(sym: &SymbolRef, krate: &Crate, root: &Path, ws: &Workspace) -> String {
    let elsewhere: Vec<String> = ws
        .crates
        .iter()
        .filter(|c| c.name != krate.name)
        .filter(|c| !find_candidates(&crate_sources(root, c), &sym.name).is_empty())
        .map(|c| c.lib_name.clone())
        .collect();

    let base = format!(
        "no `{}` in crate `{}` (anchor: {})",
        sym.name,
        krate.name,
        sym.display_path()
    );
    if elsewhere.is_empty() {
        base
    } else {
        format!("{base} — but it exists in {}", elsewhere.join(", "))
    }
}

/// Every definition of `name` among `sources`.
fn find_candidates(sources: &[SourceFile], name: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for f in sources {
        let Some(lang) = Language::from_path(&f.path) else {
            continue;
        };
        // The symbol graph finds fns/types; the measurement pass finds
        // consts/statics/enums. Neither alone covers what a spec anchors.
        for d in extract_symbols(lang, &f.source).defs {
            if d.name == name {
                out.push(Candidate {
                    path: f.path.clone(),
                    line: d.line,
                    source: f.source.clone(),
                });
            }
        }
        if !out.iter().any(|c| c.path == f.path) {
            if let Some(line) = const_line(&f.source, name) {
                out.push(Candidate {
                    path: f.path.clone(),
                    line,
                    source: f.source.clone(),
                });
            }
        }
    }
    out
}

/// The line a `const`/`static` named `name` is declared on.
///
/// `sc-index`'s query captures no consts (it builds a def/ref graph, and consts
/// are not call targets), so `Phase::ALL` is invisible to it. Rather than widen
/// that shared query for one consumer, locate consts here — the measurement pass
/// already parses this file anyway.
fn const_line(source: &str, name: &str) -> Option<usize> {
    for (i, line) in source.lines().enumerate() {
        let t = line.trim_start().trim_start_matches("pub ").trim_start();
        for kw in ["const ", "static "] {
            if let Some(rest) = t.strip_prefix(kw) {
                let rest = rest.trim_start_matches("mut ").trim_start();
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if ident == name {
                    return Some(i + 1);
                }
            }
        }
    }
    None
}

/// Every indexable source file in one crate.
fn crate_sources(root: &Path, krate: &Crate) -> Vec<SourceFile> {
    let src = krate.src_dir(root);
    if !src.is_dir() {
        return Vec::new();
    }
    // Paths come back relative to the crate's `src/`; re-root them on the
    // workspace so the report shows a path a reader can open.
    sc_index::collect_sources(&src)
        .into_iter()
        .map(|f| SourceFile {
            path: format!("{}/src/{}", krate.dir, f.path),
            source: f.source,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::{parse_anchors, AnchorKind};
    use crate::status::ClaimStatus;
    use crate::test_support::{crate_manifest, repo_root, temp_repo, workspace_manifest, write};

    /// Parse one anchor's symbol ref, for terse tests.
    fn sym(text: &str) -> (SymbolRef, Option<usize>) {
        match parse_anchors("s.md", text).remove(0).kind {
            AnchorKind::Symbol { sym } => (sym, None),
            AnchorKind::SymbolLen { sym, expect } => (sym, Some(expect)),
            other => panic!("not a symbol anchor: {other:?}"),
        }
    }

    fn one_crate_repo(tag: &str, name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = temp_repo(tag);
        write(&root, "Cargo.toml", &workspace_manifest(&[name]));
        write(
            &root,
            &format!("crates/{name}/Cargo.toml"),
            &crate_manifest(name),
        );
        for (rel, src) in files {
            write(&root, &format!("crates/{name}/src/{rel}"), src);
        }
        root
    }

    #[test]
    fn an_unknown_crate_segment_is_broken() {
        // The one unambiguous rejection: the manifest is ground truth.
        let root = one_crate_repo("sym-nocrate", "sc-a", &[("lib.rs", "pub fn x() {}\n")]);
        let ws = Workspace::load(&root);
        let (s, _) = sym("<!--@ sc_ghost::x -->");
        let r = resolve(&s, &root, &ws, None);
        assert_eq!(r.status, ClaimStatus::Broken);
        assert!(r.note.unwrap().contains("no crate `sc_ghost`"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn module_segments_that_do_not_match_the_directory_still_resolve() {
        // THE false-Broken guard. Re-exports mean use-path != file-path:
        // `sc_workflow::artifact_dirs` (a real anchor) lives in artifact_dir.rs.
        let root = one_crate_repo(
            "sym-reexport",
            "sc-workflow",
            &[
                ("lib.rs", "pub use artifact_dir::artifact_dirs;\n"),
                ("artifact_dir.rs", "pub fn artifact_dirs() {}\n"),
            ],
        );
        let ws = Workspace::load(&root);

        // Plain, and with module segments that match nothing on disk.
        for anchor in [
            "<!--@ sc_workflow::artifact_dirs -->",
            "<!--@ sc_workflow::totally::made::up::artifact_dirs -->",
        ] {
            let (s, _) = sym(anchor);
            let r = resolve(&s, &root, &ws, None);
            assert_eq!(r.status, ClaimStatus::Ok, "{anchor} → {r:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_absent_symbol_is_broken_and_names_where_it_moved_to() {
        let root = temp_repo("sym-moved");
        write(
            &root,
            "Cargo.toml",
            &workspace_manifest(&["sc-web", "sc-core"]),
        );
        write(&root, "crates/sc-web/Cargo.toml", &crate_manifest("sc-web"));
        write(&root, "crates/sc-web/src/lib.rs", "pub fn serve() {}\n");
        write(
            &root,
            "crates/sc-core/Cargo.toml",
            &crate_manifest("sc-core"),
        );
        write(
            &root,
            "crates/sc-core/src/lib.rs",
            "pub fn mint_token() -> String { String::new() }\n",
        );
        let ws = Workspace::load(&root);

        let (s, _) = sym("<!--@ sc_web::mint_token -->");
        let r = resolve(&s, &root, &ws, None);
        assert_eq!(r.status, ClaimStatus::Broken);
        let note = r.note.unwrap();
        // Enriched, not merely "not found" — a moved symbol is actionable.
        assert!(note.contains("sc_core"), "{note}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_definitions_of_one_name_are_unknown_never_broken() {
        // Ambiguity is a limit of the checker. Reporting Broken would blame the
        // spec for the checker's inability to choose.
        let root = one_crate_repo(
            "sym-ambiguous",
            "sc-a",
            &[
                ("one.rs", "pub fn parse() {}\n"),
                ("two.rs", "pub fn parse() {}\n"),
            ],
        );
        let ws = Workspace::load(&root);
        let (s, _) = sym("<!--@ sc_a::parse -->");
        let r = resolve(&s, &root, &ws, None);
        assert_eq!(r.status, ClaimStatus::Unknown);
        assert_eq!(r.targets.len(), 2, "both candidates are shown");
        assert!(r.note.unwrap().contains("ambiguous"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_crate_with_no_rust_is_unknown_not_broken() {
        let root = temp_repo("sym-norust");
        write(&root, "Cargo.toml", &workspace_manifest(&["sc-a"]));
        write(&root, "crates/sc-a/Cargo.toml", &crate_manifest("sc-a"));
        write(&root, "crates/sc-a/src/notes.md", "# not code\n");
        let ws = Workspace::load(&root);
        let (s, _) = sym("<!--@ sc_a::thing -->");
        let r = resolve(&s, &root, &ws, None);
        assert_eq!(r.status, ClaimStatus::Unknown);
        assert!(r.note.unwrap().contains("no indexable Rust"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_colliding_across_crates_resolves_per_crate() {
        // Crate scoping is what makes `main`/`run`/`parse` tractable at all.
        let root = temp_repo("sym-collide");
        write(&root, "Cargo.toml", &workspace_manifest(&["sc-a", "sc-b"]));
        write(&root, "crates/sc-a/Cargo.toml", &crate_manifest("sc-a"));
        write(&root, "crates/sc-a/src/lib.rs", "pub fn run() {}\n");
        write(&root, "crates/sc-b/Cargo.toml", &crate_manifest("sc-b"));
        write(&root, "crates/sc-b/src/lib.rs", "pub fn run() {}\n");
        let ws = Workspace::load(&root);

        for (anchor, want_dir) in [
            ("<!--@ sc_a::run -->", "crates/sc-a"),
            ("<!--@ sc_b::run -->", "crates/sc-b"),
        ] {
            let (s, _) = sym(anchor);
            let r = resolve(&s, &root, &ws, None);
            assert_eq!(r.status, ClaimStatus::Ok, "{anchor}");
            assert!(r.targets[0].path.starts_with(want_dir), "{r:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_len_assertion_checks_and_a_wrong_one_is_stale() {
        let root = one_crate_repo(
            "sym-len",
            "sc-workflow",
            &[(
                "phase.rs",
                "pub enum Phase { A, B }\nimpl Phase {\n    pub const ALL: [Phase; 5] = [A, B, C, D, E];\n}\n",
            )],
        );
        let ws = Workspace::load(&root);

        let (s, expect) = sym("<!--@ sc_workflow::Phase::ALL len=5 -->");
        let r = resolve(&s, &root, &ws, expect);
        assert_eq!(r.status, ClaimStatus::Ok, "{r:?}");
        assert!(r.targets[0].line.is_some(), "a const gets a line");

        let (s, expect) = sym("<!--@ sc_workflow::Phase::ALL len=6 -->");
        let r = resolve(&s, &root, &ws, expect);
        assert_eq!(r.status, ClaimStatus::Stale);
        assert!(r.note.unwrap().contains("len=6"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dead_array_slot_reports_stale_against_the_code() {
        let root = one_crate_repo(
            "sym-deadslot",
            "sc-workflow",
            &[(
                "phase.rs",
                "impl Phase {\n    pub const ALL: [Phase; 6] = [A, B, C, D, E];\n}\n",
            )],
        );
        let ws = Workspace::load(&root);
        // Even with the spec agreeing with the WRONG declared length.
        let (s, expect) = sym("<!--@ sc_workflow::Phase::ALL len=6 -->");
        let r = resolve(&s, &root, &ws, expect);
        assert_eq!(r.status, ClaimStatus::Stale);
        let note = r.note.unwrap();
        assert!(note.contains("inconsistent"), "{note}");
        assert!(note.contains("dead slot"), "{note}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn len_on_an_uncountable_symbol_is_unknown_not_broken() {
        // Two ways a `len=` fails to measure, and BOTH must be Unknown rather
        // than Broken: the symbol is there, so the code is not the problem.
        let root = one_crate_repo(
            "sym-uncountable",
            "sc-a",
            &[(
                "lib.rs",
                "pub fn x() {}\npub const NAME: &str = \"hello\";\n",
            )],
        );
        let ws = Workspace::load(&root);

        // A function: not a measurable item kind at all.
        let (s, expect) = sym("<!--@ sc_a::x len=3 -->");
        let r = resolve(&s, &root, &ws, expect);
        assert_eq!(r.status, ClaimStatus::Unknown, "{r:?}");
        assert!(r.note.unwrap().contains("len="), "the reason names len=");

        // A const that is measurable-in-principle but holds no collection.
        let (s, expect) = sym("<!--@ sc_a::NAME len=5 -->");
        let r = resolve(&s, &root, &ws, expect);
        assert_eq!(r.status, ClaimStatus::Unknown, "{r:?}");
        assert!(r.note.unwrap().contains("unmeasurable"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_real_symbol_anchor_in_this_repo_resolves() {
        // Against the actual workspace. All four symbol anchors in specs 17-20
        // point at real code today, so this must pass — and will fail the moment
        // one of them rots.
        let root = repo_root();
        let ws = Workspace::load(&root);
        for text in [
            "<!--@ sc_web::mint_token -->",
            "<!--@ sc_win::config::types::UiConfig -->",
            "<!--@ sc_workflow::artifact_dirs -->",
            "<!--@ sc_workflow::Phase::ALL len=5 -->",
        ] {
            let (s, expect) = sym(text);
            let r = resolve(&s, &root, &ws, expect);
            assert_eq!(r.status, ClaimStatus::Ok, "{text} → {r:?}");
        }
    }

    #[test]
    fn const_line_finds_declarations_the_symbol_graph_misses() {
        let src = "pub fn a() {}\nimpl X {\n    pub const ALL: [u8; 2] = [1, 2];\n}\n";
        assert_eq!(const_line(src, "ALL"), Some(3));
        assert_eq!(const_line("static mut COUNT: u8 = 0;", "COUNT"), Some(1));
        assert_eq!(const_line(src, "MISSING"), None);
        // Must not match a prefix.
        assert_eq!(const_line("const ALLOWED: u8 = 1;", "ALL"), None);
    }
}
