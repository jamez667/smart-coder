//! The persistent index (spec 23 — the index).
//!
//! One serde_json file at `.smart-coder/index.json`, holding a per-file record of
//! everything retrieval needs: hash, size, line count, symbols and their spans,
//! and the weighted term postings that back search. No sqlite, no tantivy — a
//! serialized struct, in the house tradition of hand-rolled and tiny.
//!
//! Two properties are load-bearing, and both are tested rather than assumed:
//!
//! * **The cache is an accelerator, never a source of truth.** A version mismatch,
//!   an unreadable file or corrupt JSON is a silent full rebuild. Deleting the file
//!   must change timing and nothing else — so no output may ever depend on whether
//!   the cache was warm.
//! * **The bytes are a pure function of the tree.** Every map is a `BTreeMap`, paths
//!   are `/`-separated and workspace-relative, and files serialize in path order, so
//!   an index built on Windows equals one built on Linux and two builds of the same
//!   tree are byte-identical.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::lexicon::{tokenize, Field};
use crate::symbols::{extract_all, Language};
use crate::walk::{walk, WalkOptions};

/// Bumped whenever a stored field changes meaning. An index written by a different
/// version is discarded rather than migrated: the cache holds nothing that cannot be
/// recomputed from the tree in a couple of seconds, so migration code would be pure
/// liability.
pub const INDEX_FORMAT_VERSION: u32 = 1;

/// Where the index lives, under the directory every walk already skips.
pub const INDEX_REL_PATH: &str = ".smart-coder/index.json";

/// A definition and the span it occupies — the unit a search hit resolves to,
/// because "this function" is something the model can act on with `read_function`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbol {
    pub name: String,
    /// 1-based line of the definition.
    pub line: usize,
    /// 1-based inclusive end line. Equals `line` for symbols with no resolvable body.
    pub end_line: usize,
    /// Whether this definition is test code.
    ///
    /// Most tests in this project are inline `#[cfg(test)] mod tests` blocks, not
    /// files under `tests/`, so a path rule alone misses them — and test names are
    /// long English sentences, exactly the shape a natural-language query matches by
    /// accident. Recorded per symbol so search can rank a test below the code it
    /// describes without pretending the test does not exist.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_test: bool,
}

impl IndexedSymbol {
    /// Number of lines the definition spans, inclusive.
    pub fn len_lines(&self) -> usize {
        self.end_line.saturating_sub(self.line) + 1
    }

    /// Whether `line` falls inside this definition.
    pub fn contains(&self, line: usize) -> bool {
        line >= self.line && line <= self.end_line
    }
}

/// One occurrence group: a term, the field it appeared in, how often, and the line
/// it anchors to.
///
/// **Anchored to the enclosing definition, not to the raw line.** Search aggregates
/// per symbol span anyway — a hit is "this function", the unit a model can act on
/// with `read_function` — so per-line detail is resolution nobody reads, and it is
/// most of the file: collapsing it cut this repo's postings from 716k to 397k. A
/// term outside any definition (a module-level comment, a `.md` file) anchors to its
/// own line, which is the best answer available and what a reader wants anyway.
///
/// Serialized with short field names because this struct *is* the file: 400k copies
/// of `"term"`/`"field"`/`"line"`/`"count"` is megabytes of nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    #[serde(rename = "t")]
    pub term: String,
    #[serde(rename = "f")]
    pub field: Field,
    /// 1-based line of the enclosing definition, or of the occurrence itself when it
    /// sits outside every definition.
    #[serde(rename = "l")]
    pub line: usize,
    #[serde(rename = "n")]
    pub count: u32,
}

/// Everything the index knows about one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Content hash, hex. The staleness check of last resort.
    pub hash: String,
    pub size: u64,
    /// Filesystem mtime as whole seconds since the epoch; `None` when unavailable.
    pub mtime: Option<u64>,
    pub lines: usize,
    /// `"rust"`, `"python"`, `"csharp"`, or `None` for a file no grammar parses.
    pub language: Option<String>,
    pub symbols: Vec<IndexedSymbol>,
    /// Distinct identifiers this file references, feeding the PageRank graph.
    pub refs: Vec<String>,
    pub postings: Vec<Posting>,
    /// TODO/FIXME count, for the health report.
    pub todos: usize,
}

/// A persistent, incrementally-refreshed snapshot of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIndex {
    pub version: u32,
    /// Per-file records, keyed by workspace-relative `/`-separated path. A `BTreeMap`
    /// so serialization order is path order, always.
    pub files: BTreeMap<String, FileRecord>,
    /// Not serialized: where this index was built, so queries can read source back.
    #[serde(skip)]
    root: PathBuf,
    /// Not serialized: files parsed during the last [`RepoIndex::open`], so a test can
    /// observe that touching one file re-parses exactly one record.
    #[serde(skip)]
    parsed_this_open: usize,
}

impl RepoIndex {
    /// Load the cached index, refresh what changed, save, and return it.
    ///
    /// Cheap enough to call per tool invocation: a warm open hashes nothing it can
    /// rule out by size and mtime. Never fails — an unwritable workspace yields a
    /// perfectly good in-memory index that simply is not cached.
    pub fn open(root: &Path) -> RepoIndex {
        let mut idx = Self::load(root).unwrap_or_else(|| RepoIndex::empty(root));
        idx.root = root.to_path_buf();
        idx.refresh();
        idx.save();
        idx
    }

    /// Build a fresh index without reading or writing the cache — the reference
    /// implementation the cached path must agree with byte-for-byte.
    pub fn build(root: &Path) -> RepoIndex {
        let mut idx = RepoIndex::empty(root);
        idx.refresh();
        idx
    }

    fn empty(root: &Path) -> RepoIndex {
        RepoIndex {
            version: INDEX_FORMAT_VERSION,
            files: BTreeMap::new(),
            root: root.to_path_buf(),
            parsed_this_open: 0,
        }
    }

    /// The workspace this index describes.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How many files were parsed during the last refresh. Zero on a fully warm
    /// open; one after touching one file.
    pub fn parsed_count(&self) -> usize {
        self.parsed_this_open
    }

    /// Read the cache, or `None` if it is absent, unreadable, corrupt, or written by
    /// another format version. Every one of those is the same event: rebuild.
    fn load(root: &Path) -> Option<RepoIndex> {
        let raw = std::fs::read_to_string(root.join(INDEX_REL_PATH)).ok()?;
        let idx: RepoIndex = serde_json::from_str(&raw).ok()?;
        (idx.version == INDEX_FORMAT_VERSION).then_some(idx)
    }

    /// Write the cache, best-effort. A read-only workspace is not an error: the index
    /// works, it just rebuilds next time.
    fn save(&self) {
        let path = self.root.join(INDEX_REL_PATH);
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        if let Ok(json) = self.to_json() {
            let _ = std::fs::write(&path, json);
        }
    }

    /// The index as JSON — the byte-for-byte determinism surface.
    ///
    /// Compact, not pretty. Nobody reads this file by hand (`smart-coder index`
    /// reports on it), and pretty-printing 400k postings spends ~120 bytes per
    /// posting on whitespace and repeated field names — tens of megabytes to
    /// indent a cache.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Re-parse changed files, drop deleted ones, keep everything else.
    fn refresh(&mut self) {
        self.parsed_this_open = 0;
        let mut seen: BTreeMap<String, FileRecord> = BTreeMap::new();
        for file in walk(&self.root, &WalkOptions::default()) {
            let mtime = mtime_secs(&file.abs);
            // Fast path: same size and mtime as the cached record means unchanged.
            // Cheap and wrong only when a file is rewritten within the same second at
            // exactly the same length -- which the hash below catches on the next
            // touch, and which never survives an edit that changes anything.
            if let Some(prev) = self.files.get(&file.rel) {
                if prev.size == file.size && prev.mtime == mtime && mtime.is_some() {
                    seen.insert(file.rel, prev.clone());
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(&file.abs) else {
                // Binary or unreadable: not index material, and not an error.
                continue;
            };
            let hash = hash_of(&text);
            // Slow path confirmed unchanged: same content, only metadata moved.
            if let Some(prev) = self.files.get(&file.rel) {
                if prev.hash == hash {
                    let mut rec = prev.clone();
                    rec.size = file.size;
                    rec.mtime = mtime;
                    seen.insert(file.rel, rec);
                    continue;
                }
            }
            self.parsed_this_open += 1;
            seen.insert(
                file.rel.clone(),
                build_record(&file.rel, &text, file.size, mtime, hash),
            );
        }
        self.files = seen;
    }

    /// Every indexed path, in order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(|k| k.as_str())
    }

    /// The symbol whose span encloses `line` in `path`, preferring the innermost.
    pub fn enclosing_symbol(&self, path: &str, line: usize) -> Option<&IndexedSymbol> {
        self.files
            .get(path)?
            .symbols
            .iter()
            .filter(|s| s.contains(line))
            .min_by_key(|s| s.len_lines())
    }
}

/// Whole seconds since the epoch, so an index is not invalidated by sub-second
/// precision differing between filesystems.
fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn hash_of(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Parse one file into its record. Never fails: an unparseable file still gets line
/// counts and code-field postings, which is strictly better than not being indexed.
fn build_record(rel: &str, text: &str, size: u64, mtime: Option<u64>, hash: String) -> FileRecord {
    // Normalize line endings so a CRLF checkout and an LF one produce identical
    // spans and line numbers.
    let src = text.replace("\r\n", "\n").replace('\r', "\n");
    let lang = Language::from_path(rel);
    let rel_is_test = path_is_test(rel);
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    if let Some(lang) = lang {
        // One parse for spans AND references: two parses of every file cost 2.5s of
        // a 3.1s build, and asking `function_span` per symbol was far worse still
        // (~13s on this repo's largest file alone).
        let (spans, r) = extract_all(lang, &src);
        refs = r;
        let test_spans = test_regions(&src, &spans);
        symbols = spans
            .into_iter()
            .map(|(name, line, end_line)| IndexedSymbol {
                is_test: rel_is_test || test_spans.iter().any(|(s, e)| line >= *s && line <= *e),
                name,
                line,
                end_line,
            })
            .collect();
        symbols.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
        symbols.dedup();
    }
    FileRecord {
        hash,
        size,
        mtime,
        lines: src.lines().count(),
        language: lang.map(|l| match l {
            Language::Rust => "rust".to_string(),
            Language::Python => "python".to_string(),
            Language::CSharp => "csharp".to_string(),
        }),
        postings: build_postings(&src, &symbols),
        symbols,
        refs,
        todos: count_todos(&src),
    }
}

/// Whether a workspace-relative path is a test file by convention.
pub fn path_is_test(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("test_")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains(".spec.")
}

/// Line spans covered by test code: `#[cfg(test)]` modules and `#[test]`/`#[bench]`
/// functions, plus Python `def test_*`.
///
/// Attribute-driven rather than name-driven, because the attribute is what actually
/// makes something a test. A `#[cfg(test)] mod tests` block claims every definition
/// inside it, which is how the bulk of this project's tests are written.
fn test_regions(src: &str, spans: &[(String, usize, usize)]) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        let marks_test = t.starts_with("#[cfg(test)]")
            || t.starts_with("#[test]")
            || t.starts_with("#[bench]")
            || t.starts_with("#[tokio::test]");
        if !marks_test {
            continue;
        }
        // The attribute belongs to the next definition that starts at or below it.
        if let Some((_, s, e)) = spans
            .iter()
            .filter(|(_, s, _)| *s > i)
            .min_by_key(|(_, s, _)| *s)
        {
            out.push((*s, *e));
        }
    }
    // Python has no attribute; the convention is the name.
    for (name, s, e) in spans {
        if name.starts_with("test_") {
            out.push((*s, *e));
        }
    }
    out
}

fn count_todos(src: &str) -> usize {
    src.lines()
        .filter(|l| {
            let u = l.to_ascii_uppercase();
            u.contains("TODO") || u.contains("FIXME")
        })
        .count()
}

/// Tokenize a file into weighted, line-located postings.
///
/// Field classification is line-oriented and language-agnostic on purpose. A
/// tree-sitter pass could label every comment and string node exactly, but it would
/// have to be written per grammar and would still fall back to this for the files no
/// grammar covers (`.md`, `.toml`, `.sh`) — which are precisely the files where prose
/// matters most. One classifier, used everywhere, keeps the index honest about what
/// it actually knows.
fn build_postings(src: &str, symbols: &[IndexedSymbol]) -> Vec<Posting> {
    // term -> field -> anchor line -> count, all ordered so the output is deterministic.
    let mut acc: BTreeMap<(String, Field, usize), u32> = BTreeMap::new();
    let doc_owner = doc_comment_owners(src, symbols);

    for (i, raw) in src.lines().enumerate() {
        let line = anchor_line(symbols, &doc_owner, i + 1);
        let (code, comment, strings) = split_line(raw);
        for (text, field) in [
            (comment, Field::Comment),
            (strings, Field::Str),
            (code, Field::Code),
        ] {
            for term in tokenize(&text) {
                *acc.entry((term, field, line)).or_insert(0) += 1;
            }
        }
    }
    // A definition's own name is the strongest signal a file gives about what it is,
    // and it is recorded at the definition's line so a hit resolves to that symbol.
    for s in symbols {
        for term in tokenize(&s.name) {
            *acc.entry((term, Field::Symbol, s.line)).or_insert(0) += 1;
        }
    }

    acc.into_iter()
        .map(|((term, field, line), count)| Posting {
            term,
            field,
            line,
            count,
        })
        .collect()
}

/// How many lines of comment directly above a definition still count as part of it.
///
/// A doc comment is not inside the definition's span, but it is unambiguously *about*
/// it — and it is the single most valuable text in the file for retrieval, because it
/// is where the author wrote in the user's language rather than the compiler's.
/// Anchoring it to the definition is what lets "why does the screen flicker with a
/// stale buffer" return `fn commit_frame` instead of a bare line number.
///
/// Bounded rather than unbounded: a long licence header at the top of a file is not a
/// description of whatever function happens to follow it.
const DOC_COMMENT_LOOKAHEAD: usize = 24;

/// The line a term at `line` is recorded against: the innermost definition enclosing
/// it, else the definition its doc comment introduces, else `line` itself.
fn anchor_line(
    symbols: &[IndexedSymbol],
    doc_owner: &BTreeMap<usize, usize>,
    line: usize,
) -> usize {
    if let Some(s) = symbols
        .iter()
        .filter(|s| s.contains(line))
        .min_by_key(|s| s.len_lines())
    {
        return s.line;
    }
    doc_owner.get(&line).copied().unwrap_or(line)
}

/// Map each comment line in a run directly above a definition to that definition's
/// line. Only *contiguous* comment lines count, so a blank line ends the association.
fn doc_comment_owners(src: &str, symbols: &[IndexedSymbol]) -> BTreeMap<usize, usize> {
    let lines: Vec<&str> = src.lines().collect();
    let mut owners = BTreeMap::new();
    for s in symbols {
        // Walk upward from the definition over contiguous comment lines.
        let mut l = s.line;
        let mut steps = 0usize;
        while l > 1 && steps < DOC_COMMENT_LOOKAHEAD {
            let above = lines[l - 2].trim();
            let is_comment = above.starts_with("//")
                || above.starts_with('#')
                || above.starts_with("/*")
                || above.starts_with('*');
            if !is_comment {
                break;
            }
            // An inner definition claims its own doc comment; the outer one does not
            // steal it back.
            owners.entry(l - 1).or_insert(s.line);
            l -= 1;
            steps += 1;
        }
    }
    owners
}

/// Split one line into `(code, comment, string-literals)`.
///
/// A hand-rolled scanner rather than a regex: the `regex` crate has no look-around,
/// and the rule here ("a `//` outside a string starts a comment") is a two-state
/// machine that is clearer written out than encoded in a pattern.
fn split_line(line: &str) -> (String, String, String) {
    let mut code = String::new();
    let mut comment = String::new();
    let mut strings = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(quote) = in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == quote {
                in_string = None;
            } else {
                strings.push(c);
            }
            i += 1;
            continue;
        }
        // Comment openers: `//` and `#` (Python/TOML/shell) and `--` (SQL-ish).
        // `/*` is treated as a line comment start too: a block comment's opening
        // line is prose, and its continuation lines have no code on them anyway.
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if two == "//" || two == "/*" || two == "--" || c == '#' {
            comment.push_str(&chars[i..].iter().collect::<String>());
            break;
        }
        if c == '"' || c == '\'' {
            in_string = Some(c);
            i += 1;
            continue;
        }
        code.push(c);
        i += 1;
    }
    (code, comment, strings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-index-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn indexes_symbols_with_spans_and_line_counts() {
        let root = temp_repo("basic");
        write(
            &root,
            "src/a.rs",
            "fn first() {\n    let x = 1;\n}\n\nfn second() {}\n",
        );
        let idx = RepoIndex::build(&root);
        let rec = idx.files.get("src/a.rs").expect("file indexed");
        assert_eq!(rec.lines, 5);
        assert_eq!(rec.language.as_deref(), Some("rust"));
        let names: Vec<&str> = rec.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
        assert!(rec.symbols.iter().all(|s| !s.is_test));
        // The span covers the whole body, not just the signature.
        assert_eq!(rec.symbols[0].line, 1);
        assert_eq!(rec.symbols[0].end_line, 3);
        assert_eq!(idx.enclosing_symbol("src/a.rs", 2).unwrap().name, "first");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The bytes are a pure function of the tree.** If this fails, the retrieval
    /// eval stops being falsifiable and a probe comparison stops meaning anything.
    #[test]
    fn two_builds_of_the_same_tree_are_byte_identical() {
        let root = temp_repo("determinism");
        write(
            &root,
            "src/a.rs",
            "// draws the trail\nfn draw_trails() {}\n",
        );
        write(&root, "src/b.py", "def helper():\n    return 'a string'\n");
        write(&root, "notes.md", "# Notes\nSome prose about trails.\n");

        let a = RepoIndex::build(&root).to_json().unwrap();
        let b = RepoIndex::build(&root).to_json().unwrap();
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The cache is an accelerator, never a source of truth.** Deleting it may
    /// change timing and nothing else.
    #[test]
    fn a_cold_open_and_a_warm_open_agree_byte_for_byte() {
        let root = temp_repo("cache");
        write(&root, "src/a.rs", "fn a() {}\n");
        write(&root, "src/b.rs", "fn b() { a(); }\n");

        let cold = RepoIndex::open(&root).to_json().unwrap();
        assert!(root.join(INDEX_REL_PATH).exists(), "cache was written");
        let warm = RepoIndex::open(&root).to_json().unwrap();
        assert_eq!(cold, warm);

        std::fs::remove_file(root.join(INDEX_REL_PATH)).unwrap();
        let rebuilt = RepoIndex::open(&root).to_json().unwrap();
        assert_eq!(cold, rebuilt, "deleting the cache changed an output");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_or_stale_cache_rebuilds_silently() {
        let root = temp_repo("corrupt");
        write(&root, "src/a.rs", "fn a() {}\n");
        let good = RepoIndex::open(&root).to_json().unwrap();

        write(&root, INDEX_REL_PATH, "{ this is not json");
        assert_eq!(RepoIndex::open(&root).to_json().unwrap(), good);

        write(&root, INDEX_REL_PATH, r#"{"version":999999,"files":{}}"#);
        assert_eq!(RepoIndex::open(&root).to_json().unwrap(), good);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Incrementality is observable, not asserted.** The parse counter is the only
    /// honest way to show that a warm open does no work.
    #[test]
    fn touching_one_file_reparses_exactly_one_record() {
        let root = temp_repo("incremental");
        write(&root, "a.rs", "fn a() {}\n");
        write(&root, "b.rs", "fn b() {}\n");
        write(&root, "c.rs", "fn c() {}\n");

        assert_eq!(RepoIndex::open(&root).parsed_count(), 3, "cold: all three");
        assert_eq!(RepoIndex::open(&root).parsed_count(), 0, "warm: none");

        // Change one file's CONTENT and its size, so the fast path cannot miss it
        // even if mtime granularity is coarse.
        write(&root, "b.rs", "fn b() { let changed = 1; }\n");
        let idx = RepoIndex::open(&root);
        assert_eq!(idx.parsed_count(), 1, "exactly the touched file");
        assert!(idx.files["b.rs"]
            .postings
            .iter()
            .any(|p| p.term == "changed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A `#[cfg(test)] mod tests` block is test code even in a source file.** Most
    /// of this project's tests live inline, so a path rule alone would call them
    /// production code and let their long English names outrank the code they test.
    #[test]
    fn inline_test_modules_are_marked_as_tests() {
        let root = temp_repo("inlinetests");
        write(
            &root,
            "src/a.rs",
            "pub fn truncate_result() {}

#[cfg(test)]
mod tests {
    #[test]
    fn a_result_that_was_cut_off_is_reported() {}
}
",
        );
        let idx = RepoIndex::build(&root);
        let by = |n: &str| {
            idx.files["src/a.rs"]
                .symbols
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("no symbol {n}"))
                .is_test
        };
        assert!(!by("truncate_result"), "production fn is not a test");
        assert!(by("a_result_that_was_cut_off_is_reported"), "inline test");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_file_drops_out() {
        let root = temp_repo("deleted");
        write(&root, "a.rs", "fn a() {}\n");
        write(&root, "b.rs", "fn b() {}\n");
        assert_eq!(RepoIndex::open(&root).files.len(), 2);
        std::fs::remove_file(root.join("b.rs")).unwrap();
        let idx = RepoIndex::open(&root);
        assert_eq!(idx.files.len(), 1);
        assert!(!idx.files.contains_key("b.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn comments_and_strings_are_indexed_as_their_own_fields() {
        let root = temp_repo("fields");
        write(
            &root,
            "a.rs",
            "// the trail looks thin here\nfn draw() { let msg = \"thick banner\"; }\n",
        );
        let idx = RepoIndex::build(&root);
        let p = &idx.files["a.rs"].postings;
        let field_of = |term: &str| p.iter().find(|x| x.term == term).map(|x| x.field);
        assert_eq!(field_of("thin"), Some(Field::Comment));
        assert_eq!(field_of("thick"), Some(Field::Str));
        assert_eq!(field_of("msg"), Some(Field::Code));
        // The definition's name is recorded as a Symbol posting at its line.
        assert!(p
            .iter()
            .any(|x| x.term == "draw" && x.field == Field::Symbol && x.line == 2));
        // And the string inside `draw`'s body anchors to `draw`'s line, not its own.
        assert_eq!(
            p.iter().find(|x| x.term == "thick").map(|x| x.line),
            Some(2)
        );
        // So does the comment ABOVE it: a doc comment describes the definition it
        // introduces, and is the most valuable text in the file for retrieval.
        assert_eq!(p.iter().find(|x| x.term == "thin").map(|x| x.line), Some(2));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_doc_comment_belongs_to_the_definition_below_it_but_a_header_does_not() {
        let root = temp_repo("doccomment");
        write(
            &root,
            "a.rs",
            "//! A licence-ish module header mentioning zebra.

// describes the commit
pub fn commit_frame() {
    swap();
}
",
        );
        let idx = RepoIndex::build(&root);
        let p = &idx.files["a.rs"].postings;
        // "describes" sits on line 3, directly above the fn on line 4: it anchors there.
        assert_eq!(
            p.iter().find(|x| x.term == "describe").map(|x| x.line),
            Some(4)
        );
        // The module header is separated by a blank line and belongs to nobody.
        assert_eq!(
            p.iter().find(|x| x.term == "zebra").map(|x| x.line),
            Some(1)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_not_a_comment() {
        let (code, comment, strings) = split_line(r#"let url = "https://example.com/x"; // real"#);
        assert!(comment.contains("real"), "{comment:?}");
        assert!(strings.contains("example"), "{strings:?}");
        assert!(!strings.contains("real"), "{strings:?}");
        assert!(code.contains("url"), "{code:?}");
    }

    #[test]
    fn unparseable_and_unknown_languages_still_get_records() {
        let root = temp_repo("unknown");
        write(&root, "notes.md", "# Trails\nthe trail is thin\n");
        write(&root, "broken.rs", "@@@ not ::: rust {{{\n");
        let idx = RepoIndex::build(&root);
        assert_eq!(idx.files["notes.md"].language, None);
        assert!(idx.files["notes.md"]
            .postings
            .iter()
            .any(|p| p.term == "trail"));
        assert!(idx.files["broken.rs"].symbols.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crlf_and_lf_checkouts_index_identically() {
        let crlf = temp_repo("crlf");
        let lf = temp_repo("lf");
        write(&crlf, "a.rs", "// trail\r\nfn draw_trails() {}\r\n");
        write(&lf, "a.rs", "// trail\nfn draw_trails() {}\n");
        let a = RepoIndex::build(&crlf);
        let b = RepoIndex::build(&lf);
        assert_eq!(a.files["a.rs"].lines, b.files["a.rs"].lines);
        assert_eq!(a.files["a.rs"].symbols, b.files["a.rs"].symbols);
        assert_eq!(a.files["a.rs"].postings, b.files["a.rs"].postings);
        let _ = std::fs::remove_dir_all(&crlf);
        let _ = std::fs::remove_dir_all(&lf);
    }

    /// **The warm path must actually be warm.** Asserted loosely, an order of
    /// magnitude wide, because CI hardware varies and a tight timing assertion is a
    /// flaky test that gets deleted. Measured on the real repo (619 files) at the
    /// time of writing: 2.6s cold, 171ms warm.
    #[test]
    fn a_warm_open_is_far_cheaper_than_a_cold_build() {
        let root = temp_repo("perf");
        for i in 0..60 {
            write(
                &root,
                &format!("src/m{i}.rs"),
                "// a comment about trails
fn draw(x: u32) -> u32 {
    x + 1
}
",
            );
        }
        let t = std::time::Instant::now();
        let cold = RepoIndex::open(&root);
        let cold_ms = t.elapsed().as_millis().max(1);
        assert_eq!(cold.parsed_count(), 60);

        let t = std::time::Instant::now();
        let warm = RepoIndex::open(&root);
        let warm_ms = t.elapsed().as_millis().max(1);
        assert_eq!(warm.parsed_count(), 0, "a warm open parses nothing");
        assert!(
            warm_ms * 3 <= cold_ms.max(3),
            "warm open ({warm_ms}ms) should be well under cold ({cold_ms}ms)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_index_never_indexes_itself() {
        let root = temp_repo("selfindex");
        write(&root, "a.rs", "fn a() {}\n");
        RepoIndex::open(&root);
        let idx = RepoIndex::open(&root);
        assert!(
            idx.paths().all(|p| !p.starts_with(".smart-coder")),
            "the cache must never appear in its own index"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
