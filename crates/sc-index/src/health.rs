//! Line counts and size smells (spec 23 — line counts and smells).
//!
//! A deterministic size-and-attention report, computed entirely from what the index
//! already holds. **Not a linter**: no style opinions, no model calls, no configurable
//! rule packs. It answers "what is big enough to be worth a second look", and judgment
//! about whether the code is *good* lives in `sc-review` (spec 16).
//!
//! Nothing here feeds search ranking. Letting file size move a search result is an
//! unmeasured idea, and unmeasured ranking changes are exactly what the sorted-map
//! lesson warns against.

use crate::store::RepoIndex;

/// A file over this many lines is worth a look.
pub const FILE_WARN_LINES: usize = 500;

/// A file over this many lines should be split.
pub const FILE_SPLIT_LINES: usize = 1000;

/// A function longer than this is "giant".
///
/// The same threshold `read_function` uses to nudge the model toward `edit_lines`
/// rather than a whole-function rewrite. One constant, so the tool's advice and this
/// report can never drift into disagreeing about what "large" means.
pub const GIANT_FN_LINES: usize = 120;

/// How much attention a file wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Size {
    Ok,
    /// Over [`FILE_WARN_LINES`].
    Warn,
    /// Over [`FILE_SPLIT_LINES`].
    Split,
}

impl Size {
    fn of(lines: usize) -> Size {
        if lines > FILE_SPLIT_LINES {
            Size::Split
        } else if lines > FILE_WARN_LINES {
            Size::Warn
        } else {
            Size::Ok
        }
    }

    /// The label used in the report.
    pub fn label(self) -> &'static str {
        match self {
            Size::Ok => "ok",
            Size::Warn => "warn",
            Size::Split => "SPLIT",
        }
    }
}

/// One file's size signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHealth {
    pub path: String,
    pub lines: usize,
    pub size: Size,
    pub functions: usize,
    /// Functions over [`GIANT_FN_LINES`], as `(name, length)`, longest first.
    pub giants: Vec<(String, usize)>,
    pub todos: usize,
}

/// The whole workspace's size picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub files: usize,
    pub lines: usize,
    pub functions: usize,
    pub todos: usize,
    /// Only the files with something to say: over a threshold, holding a giant
    /// function, or carrying a TODO. Sorted by lines descending, then path.
    pub notable: Vec<FileHealth>,
}

/// Compute the report from an index.
pub fn health(index: &RepoIndex) -> Health {
    let mut out = Health {
        files: index.files.len(),
        lines: 0,
        functions: 0,
        todos: 0,
        notable: Vec::new(),
    };
    for (path, rec) in &index.files {
        out.lines += rec.lines;
        out.todos += rec.todos;
        // Test symbols are excluded from the function count and from giant-hunting.
        // A long test is a long test; splitting it is not the same kind of advice as
        // splitting a 300-line function that production code calls.
        let real: Vec<_> = rec.symbols.iter().filter(|s| !s.is_test).collect();
        out.functions += real.len();

        let mut giants: Vec<(String, usize)> = real
            .iter()
            .filter(|s| s.len_lines() > GIANT_FN_LINES)
            .map(|s| (s.name.clone(), s.len_lines()))
            .collect();
        giants.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let size = Size::of(rec.lines);
        if size != Size::Ok || !giants.is_empty() || rec.todos > 0 {
            out.notable.push(FileHealth {
                path: path.clone(),
                lines: rec.lines,
                size,
                functions: real.len(),
                giants,
                todos: rec.todos,
            });
        }
    }
    // Biggest first, ties by path: deterministic, and the order a reader wants.
    out.notable
        .sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Render the report for a human.
pub fn render_health(h: &Health) -> String {
    let mut out = format!(
        "{} files · {} lines · {} functions · {} TODO/FIXME\n",
        h.files, h.lines, h.functions, h.todos
    );
    let split = h.notable.iter().filter(|f| f.size == Size::Split).count();
    let warn = h.notable.iter().filter(|f| f.size == Size::Warn).count();
    out.push_str(&format!(
        "{split} over {FILE_SPLIT_LINES} lines (split) · {warn} over {FILE_WARN_LINES} lines (warn)\n"
    ));
    if h.notable.is_empty() {
        out.push_str("\nnothing over a threshold.\n");
        return out.trim_end().to_string();
    }
    out.push('\n');
    for f in &h.notable {
        out.push_str(&format!(
            "{:>6}  {:<6} {}  ({} fn)\n",
            f.lines,
            f.size.label(),
            f.path,
            f.functions
        ));
        for (name, len) in &f.giants {
            out.push_str(&format!("        giant fn: {name} ({len} lines)\n"));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-index-health-{tag}-{}-{}",
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

    /// A function of `n` body lines, named `name`.
    fn long_fn(name: &str, n: usize) -> String {
        let body = "    let x = 1;\n".repeat(n);
        format!("pub fn {name}() {{\n{body}}}\n")
    }

    #[test]
    fn flags_files_by_size_and_functions_by_length() {
        let root = temp_repo("sizes");
        write(&root, "small.rs", &long_fn("a", 5));
        write(&root, "warn.rs", &long_fn("b", 600));
        write(&root, "split.rs", &long_fn("c", 1200));

        let h = health(&RepoIndex::build(&root));
        let by = |p: &str| h.notable.iter().find(|f| f.path == p).unwrap();
        assert_eq!(by("warn.rs").size, Size::Warn);
        assert_eq!(by("split.rs").size, Size::Split);
        assert!(!h.notable.iter().any(|f| f.path == "small.rs"));
        // Both long files hold one giant function, named and measured.
        assert_eq!(by("warn.rs").giants.len(), 1);
        assert_eq!(by("warn.rs").giants[0].0, "b");
        assert!(by("warn.rs").giants[0].1 > GIANT_FN_LINES);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A long test is a long test. Recommending a split of one is not the same advice
    /// as recommending a split of production code, so tests stay out of the counts.
    #[test]
    fn test_code_is_not_counted_as_a_giant_function() {
        let root = temp_repo("tests");
        let body = "        let x = 1;\n".repeat(200);
        write(
            &root,
            "a.rs",
            &format!(
                "pub fn real() {{}}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn enormous() {{\n{body}    }}\n}}\n"
            ),
        );
        let h = health(&RepoIndex::build(&root));
        let f = h.notable.iter().find(|f| f.path == "a.rs");
        assert!(
            f.is_none_or(|f| f.giants.is_empty()),
            "a giant TEST is not a giant function: {f:?}"
        );
        assert_eq!(h.functions, 1, "only the production fn is counted");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn counts_todos_and_reports_them() {
        let root = temp_repo("todos");
        write(
            &root,
            "a.rs",
            "// TODO: fix this\npub fn a() {}\n// FIXME: and this\n",
        );
        let h = health(&RepoIndex::build(&root));
        assert_eq!(h.todos, 2);
        assert_eq!(h.notable[0].todos, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_clean_repo_says_so_rather_than_printing_nothing() {
        let root = temp_repo("clean");
        write(&root, "a.rs", "pub fn a() {}\n");
        let out = render_health(&health(&RepoIndex::build(&root)));
        assert!(out.contains("nothing over a threshold"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_report_is_deterministic() {
        let root = temp_repo("determinism");
        write(&root, "a.rs", &long_fn("a", 600));
        write(&root, "b.rs", &long_fn("b", 600));
        let idx = RepoIndex::build(&root);
        assert_eq!(render_health(&health(&idx)), render_health(&health(&idx)));
        // Ties on line count break on path, so the order is never an accident.
        let h = health(&idx);
        assert_eq!(h.notable[0].path, "a.rs");
        assert_eq!(h.notable[1].path, "b.rs");
        let _ = std::fs::remove_dir_all(&root);
    }
}
