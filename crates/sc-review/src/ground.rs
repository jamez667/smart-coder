//! Grounding: **retrieve first, then ask** (spec 16).
//!
//! This inverts the usual order and is the point of the whole spec. A swarm
//! worker today gets its subtask goal and the text of its own files and nothing
//! else — `sc-swarm` has no repo map, because it does not depend on `sc-index`.
//! So "I couldn't find the existing helper, so I wrote one" is not a lapse, it is
//! the expected behaviour of a correctly-working worker. Handing the reviewer
//! *only the diff* would give it a strictly smaller keyhole and then ask it a
//! harder question.
//!
//! Two kinds of grounding, both retrieved before any model call:
//!
//! * the **repo map** for every lens — the PageRank structural view;
//! * for duplication, a pre-retrieved list of **existing symbols that resemble
//!   the ones the diff adds**. The model is then asked the part only it can
//!   answer — "is this the same thing?" — rather than "does something like this
//!   exist?", which the index answers better. This is also why duplication's
//!   corroboration is nearly free: the lookup already ran to build the prompt.

use std::path::Path;

use sc_index::{Boosts, SymbolHit};

use crate::diff::IntegratedDiff;

/// An existing definition that resembles something the diff adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarSymbol {
    /// The name the diff adds.
    pub added: String,
    /// The pre-existing definition that resembles it.
    pub existing: SymbolHit,
}

impl SimilarSymbol {
    /// The one-line form used both in the prompt and, when the model agrees, as
    /// the finding's evidence — so a retry prompt names a real file and line.
    pub fn describe(&self) -> String {
        format!(
            "`{}` already exists at {}:{}",
            self.existing.name, self.existing.path, self.existing.line
        )
    }
}

/// What a review call is given besides the diff.
#[derive(Debug, Clone, Default)]
pub struct Grounding {
    /// The PageRank repo map — the structural view the worker never had.
    pub repo_map: String,
    /// Pre-retrieved lookalikes for the symbols the diff adds. Duplication only.
    pub similar: Vec<SimilarSymbol>,
    /// Full text of each changed file after the change, `(path, source)`. The
    /// abstraction-fit lens needs the surrounding code: "does this match how the
    /// code around it solves this?" is unanswerable from changed lines alone.
    pub surrounding: Vec<(String, String)>,
}

/// How much of the repo map to retrieve. Enough to be a map, bounded so a
/// four-lens review doesn't ship the repository four times.
const REPO_MAP_TOP_K: usize = 60;

/// Build the grounding for one integrated diff.
///
/// `root` is the real workspace, already holding the integrated change — so a
/// symbol the diff *added* is itself in the index. That matters: a lookalike
/// search would otherwise "find" the new definition and corroborate every finding
/// against itself, so hits inside the diff's own changed files are excluded.
pub fn ground(root: &Path, diff: &IntegratedDiff) -> Grounding {
    let added = added_symbol_names(diff);
    let boosts = Boosts {
        mentioned_symbols: added.clone(),
        in_play_files: diff.files.iter().map(|f| f.path.clone()).collect(),
    };
    Grounding {
        repo_map: sc_index::repo_map(root, &boosts, REPO_MAP_TOP_K),
        similar: find_similar(root, diff, &added),
        surrounding: diff
            .files
            .iter()
            .filter_map(|f| f.after.clone().map(|src| (f.path.clone(), src)))
            .collect(),
    }
}

/// The names of symbols the diff *adds*: what is defined in each changed file
/// after the change and was not defined in it before.
///
/// The obvious implementation — parse the hunk's added lines — does not work, and
/// fails in exactly the case this whole spec is about. A function inserted into
/// the middle of a file has added lines that begin with the closing brace of the
/// function above it:
///
/// ```text
/// +    format_date(0)
/// +}
/// +fn format_date(d: u64) -> String {
/// ```
///
/// That fragment is not valid source, tree-sitter extracts nothing from it, and
/// the duplicate the reviewer exists to catch is never looked up. Comparing the
/// two whole files always parses and answers the question directly.
pub fn added_symbol_names(diff: &IntegratedDiff) -> Vec<String> {
    let defs = |lang, source: &Option<String>| -> Vec<String> {
        source
            .as_deref()
            .map(|s| {
                sc_index::extract_symbols(lang, s)
                    .defs
                    .into_iter()
                    .map(|d| d.name)
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut names: Vec<String> = Vec::new();
    for f in &diff.files {
        let Some(lang) = sc_index::Language::from_path(&f.path) else {
            continue;
        };
        let was = defs(lang, &f.before);
        for name in defs(lang, &f.after) {
            if !was.contains(&name) && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// For each added symbol, the pre-existing definitions elsewhere in the repo that
/// share its name. Definitions inside the diff's own changed files are excluded:
/// the workspace already holds the integrated change, so the added symbol is in
/// the index and would otherwise match itself.
fn find_similar(root: &Path, diff: &IntegratedDiff, added: &[String]) -> Vec<SimilarSymbol> {
    let changed: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    let mut out = Vec::new();
    for name in added {
        for hit in sc_index::find_symbol_hits(root, name) {
            if changed.contains(&hit.path.as_str()) {
                continue;
            }
            out.push(SimilarSymbol {
                added: name.clone(),
                existing: hit,
            });
        }
    }
    out
}

impl Grounding {
    /// The lookalike a claimed duplicate of `symbol` would be corroborated by, if
    /// one exists. Name match is case-insensitive because a reviewer will cite
    /// `Format_Date` for `format_date` and that is not a different claim.
    pub fn lookalike(&self, symbol: &str) -> Option<&SimilarSymbol> {
        self.similar
            .iter()
            .find(|s| s.added.eq_ignore_ascii_case(symbol) || s.existing.name == symbol)
    }

    /// The pre-retrieved lookalikes, rendered for the prompt. Empty when nothing
    /// resembles anything — said plainly, so the model is not left to infer it
    /// from an absent section and guess from naming instead.
    pub fn render_similar(&self) -> String {
        if self.similar.is_empty() {
            return "(the index found no existing symbol sharing a name with anything \
                    this diff adds)"
                .to_string();
        }
        self.similar
            .iter()
            .map(|s| format!("- the diff adds `{}`; {}", s.added, s.describe()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The surrounding files, rendered for the abstraction-fit lens.
    pub fn render_surrounding(&self) -> String {
        self.surrounding
            .iter()
            .map(|(path, src)| format!("--- {path} (after the change) ---\n{src}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-review-ground-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_duplication_lookup_runs_before_the_model_and_finds_the_original() {
        // The scenario the whole spec is built around: a worker that couldn't see
        // src/utils/date.rs reimplemented format_date in src/report/render.rs.
        let root = temp_repo("dup");
        std::fs::create_dir_all(root.join("src/utils")).unwrap();
        std::fs::create_dir_all(root.join("src/report")).unwrap();
        std::fs::write(
            root.join("src/utils/date.rs"),
            "fn unrelated() {}\nfn format_date(d: u64) -> String { String::new() }\n",
        )
        .unwrap();
        let after = "fn render() {}\nfn format_date(d: u64) -> String { String::new() }\n";
        std::fs::write(root.join("src/report/render.rs"), after).unwrap();

        let diff = IntegratedDiff::from_changes([(
            "src/report/render.rs",
            Some("fn render() {}\n"),
            Some(after),
        )]);
        let g = ground(&root, &diff);

        let hit = g
            .lookalike("format_date")
            .expect("the original is retrieved");
        assert_eq!(hit.existing.path, "src/utils/date.rs");
        assert_eq!(hit.existing.line, 2);
        // Rendered for the prompt with the location a retry prompt will need.
        assert!(
            g.render_similar().contains("src/utils/date.rs:2"),
            "{}",
            g.render_similar()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_added_symbol_does_not_corroborate_against_itself() {
        // The workspace already holds the integrated change, so the added symbol
        // IS in the index. Without excluding the diff's own files, every added
        // symbol would "already exist" and every duplication finding would be
        // corroborated — the worst possible failure, since corroboration is the
        // only thing allowed to gate.
        let root = temp_repo("self");
        let after = "fn brand_new() {}\n";
        std::fs::write(root.join("only.rs"), after).unwrap();

        let diff = IntegratedDiff::from_changes([("only.rs", None, Some(after))]);
        let g = ground(&root, &diff);

        assert!(g.similar.is_empty(), "{:?}", g.similar);
        assert!(g.lookalike("brand_new").is_none());
        assert!(g.render_similar().contains("no existing symbol"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_lens_gets_the_repo_map_the_worker_never_had() {
        let root = temp_repo("map");
        std::fs::write(root.join("core.rs"), "pub fn core() {}\n").unwrap();
        std::fs::write(root.join("a.rs"), "fn a() { core(); }\n").unwrap();

        let diff = IntegratedDiff::from_changes([(
            "a.rs",
            Some("fn a() {}\n"),
            Some("fn a() { core(); }\n"),
        )]);
        let g = ground(&root, &diff);
        assert!(g.repo_map.contains("core"), "{}", g.repo_map);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn abstraction_fit_gets_the_whole_file_not_just_the_hunk() {
        let root = temp_repo("surround");
        let after = "fn a() {}\nfn b() {}\nfn c() {}\n";
        std::fs::write(root.join("a.rs"), after).unwrap();
        let diff =
            IntegratedDiff::from_changes([("a.rs", Some("fn a() {}\nfn c() {}\n"), Some(after))]);
        let g = ground(&root, &diff);

        let rendered = g.render_surrounding();
        // The hunk is one line; the grounding carries all three.
        assert!(
            rendered.contains("fn a()") && rendered.contains("fn c()"),
            "{rendered}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn added_symbols_are_those_the_file_did_not_define_before() {
        let diff = IntegratedDiff::from_changes([(
            "a.rs",
            Some("fn kept() {}\n"),
            Some("fn kept() {}\nfn fresh() {}\n"),
        )]);
        // `kept` was already defined; only `fresh` is added.
        assert_eq!(added_symbol_names(&diff), vec!["fresh".to_string()]);
    }

    #[test]
    fn a_function_inserted_mid_file_is_still_detected_as_added() {
        // Regression. The obvious implementation parses the hunk's ADDED LINES,
        // which here begin with a dangling `}` from the function above:
        //
        //   +    format_date(0)
        //   +}
        //   +fn format_date(d: u64) -> String {
        //
        // That fragment parses to nothing, so the duplicate this whole spec exists
        // to catch would never be looked up — a silent, total failure of the
        // duplication lens on the most ordinary edit there is.
        let before = "fn render() -> String {\n    String::new()\n}\n";
        let after = "fn render() -> String {\n    format_date(0)\n}\n\
                     fn format_date(d: u64) -> String {\n    String::new()\n}\n";
        let diff =
            IntegratedDiff::from_changes([("src/report/render.rs", Some(before), Some(after))]);
        assert_eq!(added_symbol_names(&diff), vec!["format_date".to_string()]);
    }

    #[test]
    fn a_symbol_merely_moved_within_a_file_is_not_an_addition() {
        // Reordering two functions changes lines but adds no symbol, so it must
        // not send the index looking for duplicates of either.
        let diff = IntegratedDiff::from_changes([(
            "a.rs",
            Some("fn one() {}\nfn two() {}\n"),
            Some("fn two() {}\nfn one() {}\n"),
        )]);
        assert!(added_symbol_names(&diff).is_empty());
    }

    #[test]
    fn an_unindexable_language_grounds_to_nothing_rather_than_failing() {
        // The index parses Rust/Python/C#. A shell-script diff must degrade to
        // "no lookalikes" — never an error, and never a false lookalike.
        let diff = IntegratedDiff::from_changes([("build.sh", Some("a\n"), Some("b\n"))]);
        assert!(added_symbol_names(&diff).is_empty());
    }
}
