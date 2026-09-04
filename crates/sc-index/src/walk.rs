//! The one workspace walk (spec 23 — the walker).
//!
//! Four walks used to exist, hand-rolled, with three disagreeing skip lists:
//! `sc_tools::source_files` (mirrored by hand in `sc_win::config::workspace`),
//! `search_code`'s own walk, and `sc_index::collect_sources`. `collect_sources`
//! skipped `.venv` but not `dist` or `.smart-coder`; `search_code` skipped
//! `.smart-coder` but no dotdir besides `.git`; only the `source_files` pair
//! skipped `dist`/`build`. Which files the model could *see* depended on which
//! code path asked — and every consumer of every list was a prompt.
//!
//! One walk, one skip list (the union of all of them), one extension policy.
//! Callers layer their own filters *on top* (the source ledger drops tests and
//! workflow artifacts; the index keeps only parseable languages), but nobody
//! re-decides which directories exist.

use std::path::{Path, PathBuf};

/// Directories never descended into, by name, at any depth: the union of the four
/// pre-spec-23 skip lists.
///
/// `target` dominates and is the reason this list is load-bearing: measured on a
/// real Rust project, 40,585 of 41,180 files under the root were build artifacts —
/// 98.5% noise burying 595 real files. `.smart-coder` matters for a subtler reason:
/// it holds the agent's own session logs, which echo every prior tool result, so a
/// walk that descends into it lets a search match the agent's own transcript
/// (observed live: a search for a function name hit the session log instead of the
/// source, wasting turns).
///
/// Any name starting with `.` is skipped too — see [`is_skipped_dir`] — so `.git`,
/// `.venv` and `.pytest_cache` are covered by rule as well as by name.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".venv",
    ".smart-coder",
    "__pycache__",
    "node_modules",
    "target",
    "dist",
    "build",
];

/// The byte cap `sc_core`'s `gather_sources` applies to the diagnostic prompt: a
/// single vendored blob or minified bundle otherwise costs more context than every
/// real file combined.
///
/// **Opt-in, not the walk's default.** It was tempting to make it the default and
/// call the walk uniformly capped, but the cap was never a *walk* policy — it was
/// one consumer's prompt-size guard, applied after the walk. Five real source files
/// in this repo exceed it, `crates/sc-core/src/agent/mod.rs` (the agent loop) among
/// them; a walk that dropped them by default would quietly delete the project's
/// biggest files from the repo map and from `find_symbol`. Callers that are
/// building a prompt ask for it by name.
pub const PROMPT_MAX_FILE_BYTES: u64 = 64 * 1024;

/// Files never indexed or searched, by exact name: machine-generated dependency
/// manifests.
///
/// A lockfile is thousands of lines nobody wrote and nobody investigates, and it is
/// dense with terms that collide with real code (every crate name in the ecosystem).
/// `Cargo.lock` alone was the second-largest contributor of index terms in this repo —
/// more than the agent loop — and a `search_code` hit inside one has never once been
/// the answer to a question.
pub const SKIP_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "poetry.lock",
    "Gemfile.lock",
    "composer.lock",
];

/// Directories holding the agent's own recorded output rather than project source.
///
/// `.smart-coder` is already skipped as a dotdir, but the recorded probe transcripts
/// live in a plain `logs/` directory — and they echo every prior tool result, so
/// indexing them lets a search match the harness's own transcript of being asked the
/// same question. Observed the first time `smart-coder search` was run on this repo:
/// the canonical starfield query returned seven copies of `investigate-probe.md`
/// above any code, because that file contains the question *and* the answer.
///
/// Skipped by directory NAME at any depth, which is blunt: a project whose real
/// source lives in a directory called `logs` would lose it. That has not happened,
/// and the alternative -- reading file contents to guess whether they are a
/// transcript -- is worse than a rule you can read.
pub const SKIP_DIR_NAMES_LOGS: &[&str] = &["logs"];

/// Whether a file `name` is one the walk refuses to yield.
pub fn is_skipped_file(name: &str) -> bool {
    SKIP_FILES.contains(&name)
}

/// Whether a directory `name` is one the walk refuses to descend into.
///
/// Dot-prefixed names are skipped as a *rule* rather than a list, which is how the
/// `source_files` pair already treated them: a new `.mypy_cache` needs no code
/// change to stay out of a prompt.
pub fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name) || SKIP_DIR_NAMES_LOGS.contains(&name)
}

/// One file found by the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// Workspace-relative, `/`-separated — identical on Windows and Linux.
    pub rel: String,
    /// The absolute path on disk, so a caller can read it without re-joining.
    pub abs: PathBuf,
    /// Size in bytes, from the directory entry's metadata.
    pub size: u64,
}

/// How a walk decides which files to keep. Directory skipping is not configurable —
/// that is the point of the shared list.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Keep only these extensions (lowercase, no dot). Empty means every extension.
    pub extensions: Vec<String>,
    /// Skip files larger than this. `None` — the default — means no cap.
    pub max_file_bytes: Option<u64>,
}

impl WalkOptions {
    /// Only files with one of `exts` (lowercase, no dot).
    pub fn with_extensions(exts: &[&str]) -> Self {
        Self {
            extensions: exts.iter().map(|e| e.to_ascii_lowercase()).collect(),
            max_file_bytes: None,
        }
    }

    /// Replace the size cap.
    pub fn max_bytes(mut self, cap: Option<u64>) -> Self {
        self.max_file_bytes = cap;
        self
    }

    fn keeps(&self, rel: &str, size: u64) -> bool {
        if let Some(cap) = self.max_file_bytes {
            if size > cap {
                return false;
            }
        }
        if self.extensions.is_empty() {
            return true;
        }
        match rel.rsplit('.').next() {
            // `rsplit` on a dotless name yields the name itself, so guard on the
            // dot actually being there — otherwise `Makefile` matches extension
            // "makefile".
            Some(ext) if rel.contains('.') => self
                .extensions
                .iter()
                .any(|e| e == &ext.to_ascii_lowercase()),
            _ => false,
        }
    }
}

/// Walk `root`, returning every kept file sorted by relative path.
///
/// Unreadable directories and entries are skipped rather than raised: one bad file
/// never breaks retrieval, and a walk that errors is a walk callers stop trusting.
/// The sort is what makes every downstream artifact — the file map, the index, a
/// search result — a pure function of the tree's contents.
pub fn walk(root: &Path, opts: &WalkOptions) -> Vec<WalkedFile> {
    let mut out: Vec<WalkedFile> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => {
                    if !is_skipped_dir(&name) {
                        stack.push(path);
                    }
                }
                Ok(ft) if ft.is_file() => {
                    if is_skipped_file(&name) {
                        continue;
                    }
                    let rel = relative(root, &path);
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    if opts.keeps(&rel, size) {
                        out.push(WalkedFile {
                            rel,
                            abs: path,
                            size,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// `path` relative to `root`, `/`-separated. An index built on Windows and one
/// built on Linux describe the same tree with the same strings.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-index-walk-{tag}-{}-{}",
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

    /// **The union property.** Every directory any of the four pre-spec-23 walks
    /// skipped is still skipped, or the unification lost coverage that a consumer
    /// was relying on.
    #[test]
    fn the_skip_list_is_the_union_of_the_four_old_lists() {
        // sc_tools::source_files + sc_win mirror: dotdirs, __pycache__,
        // node_modules, target, dist, build.
        for d in [
            ".git",
            ".smart-coder",
            ".pytest_cache",
            "__pycache__",
            "node_modules",
            "target",
            "dist",
            "build",
        ] {
            assert!(is_skipped_dir(d), "{d} must be skipped");
        }
        // sc_index::collect_sources additionally had .venv...
        assert!(is_skipped_dir(".venv"));
        // ...and search_code's walk additionally had .smart-coder.
        assert!(is_skipped_dir(".smart-coder"));
        // The agent's own recorded transcripts are not project source.
        assert!(is_skipped_dir("logs"));
        // Real directories are not skipped.
        for d in ["src", "crates", "tests", "assets", "docs"] {
            assert!(!is_skipped_dir(d), "{d} must NOT be skipped");
        }
    }

    #[test]
    fn lockfiles_are_not_source() {
        let root = temp_repo("lockfiles");
        write(
            &root,
            "Cargo.lock",
            "[[package]]
name = \"serde\"
",
        );
        write(&root, "package-lock.json", "{}");
        write(&root, "Cargo.toml", "[package]");
        write(&root, "src/a.rs", "fn a() {}");

        let files = walk(&root, &WalkOptions::default());
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["Cargo.toml", "src/a.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walks_sorted_and_relative_with_forward_slashes() {
        let root = temp_repo("sorted");
        write(&root, "z.rs", "fn z() {}");
        write(&root, "src/a.rs", "fn a() {}");
        write(&root, "src/nested/b.rs", "fn b() {}");

        let files = walk(&root, &WalkOptions::default());
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        // Sorted, and `/`-separated even though this may be Windows.
        assert_eq!(rels, vec!["src/a.rs", "src/nested/b.rs", "z.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_output_and_caches_never_appear() {
        let root = temp_repo("skips");
        write(&root, "src/main.rs", "fn main() {}");
        write(&root, "target/debug/build/stamp.rs", "// generated");
        write(&root, "node_modules/pkg/index.js", "//dep");
        write(&root, ".git/config", "[core]");
        write(&root, ".smart-coder/sessions/log.md", "prior tool output");
        write(&root, ".venv/lib/thing.py", "x = 1");
        write(&root, "dist/bundle.js", "//built");

        let files = walk(&root, &WalkOptions::default());
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["src/main.rs"], "only real source survives");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_extension_policy_filters_and_a_dotless_name_is_not_an_extension() {
        let root = temp_repo("ext");
        write(&root, "a.rs", "fn a() {}");
        write(&root, "b.py", "x = 1");
        write(&root, "notes.md", "# hi");
        write(&root, "Makefile", "all:");

        let files = walk(&root, &WalkOptions::with_extensions(&["rs", "py"]));
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["a.rs", "b.py"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A big real file is not noise.** The cap is opt-in precisely because five
    /// source files in this repo exceed it, the agent loop among them; defaulting it
    /// on would delete them from the repo map and from `find_symbol`.
    #[test]
    fn the_size_cap_is_opt_in_and_off_by_default() {
        let root = temp_repo("cap");
        write(&root, "small.rs", "fn a() {}");
        write(&root, "huge.rs", &"x".repeat(70 * 1024));

        let uncapped = walk(&root, &WalkOptions::default());
        assert_eq!(uncapped.len(), 2, "no cap by default: {uncapped:?}");

        let capped = walk(
            &root,
            &WalkOptions::default().max_bytes(Some(PROMPT_MAX_FILE_BYTES)),
        );
        let rels: Vec<&str> = capped.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["small.rs"], "asked for a cap, got one");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_directory_does_not_break_the_walk() {
        // A path that does not exist stands in for any unreadable dir: the walk
        // returns what it could see rather than failing.
        let files = walk(
            Path::new("/definitely/not/a/real/path/xyz"),
            &WalkOptions::default(),
        );
        assert!(files.is_empty());
    }
}
