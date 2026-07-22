//! [`FileTree`] — a flattened, display-ready view of the files in a workspace, for the
//! left-hand EXPLORER panel (the VS-Code-style file list). Pure data + a filesystem
//! walk, no iced types, so it is host-testable; `app.rs` renders the flat rows as a
//! clickable list and asks this module what a click selects.
//!
//! The tree is *flattened* into an indented row list (depth + is_dir + relative path)
//! rather than a nested widget graph — that keeps the renderer a trivial `for row in`
//! loop and the collapse/expand state a plain `HashSet` of collapsed dir paths.

use std::collections::HashSet;
use std::path::Path;

/// One row in the explorer: a file or directory, with its indentation depth and its
/// workspace-relative path (forward-slashed, stable across platforms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// Nesting depth from the workspace root (root entries are depth 0).
    pub depth: usize,
    /// True for a directory, false for a file.
    pub is_dir: bool,
    /// Workspace-relative path with `/` separators (e.g. `crates/city/src/sim/mod.rs`).
    pub rel: String,
    /// The leaf name shown in the row (e.g. `mod.rs`).
    pub name: String,
}

/// Directory names never worth showing (VCS, build output, tooling caches, deps, and
/// generated-asset folders). A game's `target/` is enormous and irrelevant to iterating
/// on source; `screenshots/`/`dist/` are build artifacts a `.gitignore` would exclude.
pub fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | ".git"
            | "node_modules"
            | "__pycache__"
            | ".smart-coder"
            | ".pytest_cache"
            | "screenshots"
            | "dist"
            | "build"
            // Unity-generated / non-source trees.
            | "Library"
            | "Temp"
            | "obj"
            | "Logs"
            | "UserSettings"
            | "Builds"
    ) || name.starts_with('.') && name != "."
}

/// Build the flattened explorer rows for `root`, honoring the set of `collapsed`
/// directories (a collapsed dir contributes its own row but none of its children).
///
/// Ordering at every level: directories first (so folders group at the top like an
/// IDE), then files, each alphabetical case-insensitively. `collapsed` holds
/// workspace-relative dir paths (`/`-separated); pass an empty set for fully expanded.
pub fn build_rows(root: &Path, collapsed: &HashSet<String>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    walk(root, root, 0, collapsed, &mut rows);
    rows
}

/// The workspace-relative names of the top-level (non-noise) directories under `root`. Seeding
/// these into the explorer's `collapsed` set makes the tree open *compacted* — root files and
/// folder headers show, but every folder starts closed (the user expands what they need).
pub fn top_level_dirs(root: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() && !is_noise_dir(&name) => Some(name),
                _ => None,
            }
        })
        .collect()
}

/// Filter the tree to rows matching `query` (case-insensitive substring on the leaf name or the
/// full relative path). Collapse state is ignored — a filter searches the WHOLE tree — so every
/// match shows regardless of which folders are closed. Matching files keep their ancestor
/// directories (so the path context is visible); a directory whose own name matches is kept too.
/// An empty/whitespace query returns the normal collapsed view via [`build_rows`].
pub fn filter_rows(root: &Path, collapsed: &HashSet<String>, query: &str) -> Vec<TreeRow> {
    let q = query.trim();
    if q.is_empty() {
        return build_rows(root, collapsed);
    }
    filter_view(&full_rows(root), q)
}

/// The FULLY-expanded tree (every non-noise dir walked), for caching. Walking the filesystem is
/// the expensive part; do it once and derive the collapsed/filtered views in memory with
/// [`collapse_view`] / [`filter_view`] rather than re-walking on every frame.
pub fn full_rows(root: &Path) -> Vec<TreeRow> {
    build_rows(root, &HashSet::new())
}

/// Derive the collapsed display view from an already-walked full tree: drop any row whose parent
/// directory (or any ancestor) is in `collapsed`. Pure in-memory — no filesystem access.
pub fn collapse_view(full: &[TreeRow], collapsed: &HashSet<String>) -> Vec<TreeRow> {
    if collapsed.is_empty() {
        return full.to_vec();
    }
    full.iter()
        .filter(|r| {
            // Keep a row unless one of its ancestor dirs is collapsed. (A collapsed dir keeps its
            // own row but hides its descendants.)
            let mut cur = r.rel.as_str();
            while let Some(slash) = cur.rfind('/') {
                cur = &cur[..slash];
                if collapsed.contains(cur) {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// Derive the filtered display view from an already-walked full tree: keep rows matching `query`
/// (case-insensitive substring on the leaf name or full path) plus the ancestor dirs leading to
/// each match (for context). Pure in-memory — no filesystem access.
pub fn filter_view(full: &[TreeRow], query: &str) -> Vec<TreeRow> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return full.to_vec();
    }
    let mut keep: HashSet<&str> = HashSet::new();
    for r in full {
        let hit =
            r.name.to_ascii_lowercase().contains(&q) || r.rel.to_ascii_lowercase().contains(&q);
        if hit {
            keep.insert(r.rel.as_str());
            let mut cur = r.rel.as_str();
            while let Some(slash) = cur.rfind('/') {
                cur = &cur[..slash];
                keep.insert(cur);
            }
        }
    }
    full.iter()
        .filter(|r| keep.contains(r.rel.as_str()))
        .cloned()
        .collect()
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Vec<TreeRow>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Split into dirs and files, skipping noise, so we can emit dirs-first.
    let mut dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        match entry.file_type() {
            Ok(ft) if ft.is_dir() && !is_noise_dir(&name) => dirs.push((name, path)),
            Ok(ft) if ft.is_file() => files.push((name, path)),
            _ => {}
        }
    }
    dirs.sort_by_key(|(name, _)| name.to_ascii_lowercase());
    files.sort_by_key(|(name, _)| name.to_ascii_lowercase());

    for (name, path) in dirs {
        let rel = rel_of(root, &path);
        out.push(TreeRow {
            depth,
            is_dir: true,
            rel: rel.clone(),
            name,
        });
        // Recurse only if this dir isn't collapsed.
        if !collapsed.contains(&rel) {
            walk(root, &path, depth + 1, collapsed, out);
        }
    }
    for (name, path) in files {
        out.push(TreeRow {
            depth,
            is_dir: false,
            rel: rel_of(root, &path),
            name,
        });
    }
}

/// Workspace-relative, forward-slashed path of `path` under `root`.
fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small scratch tree, returned as its root path; cleaned by the caller.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-win-ft-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates/city/src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap(); // noise
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // noise
        std::fs::write(dir.join("Cargo.toml"), "x").unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        std::fs::write(dir.join("crates/city/src/main.rs"), "x").unwrap();
        std::fs::write(dir.join("crates/city/src/sim.rs"), "x").unwrap();
        std::fs::write(dir.join("target/debug/city.exe"), "x").unwrap();
        dir
    }

    #[test]
    fn skips_target_and_git_and_lists_sources() {
        let dir = scratch("skip");
        let rows = build_rows(&dir, &HashSet::new());
        let rels: Vec<&str> = rows.iter().map(|r| r.rel.as_str()).collect();
        assert!(rels.contains(&"crates"), "{rels:?}");
        assert!(rels.contains(&"crates/city/src/main.rs"), "{rels:?}");
        assert!(rels.contains(&"Cargo.toml"), "{rels:?}");
        assert!(
            !rels.iter().any(|r| r.starts_with("target")),
            "target must be skipped: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.starts_with(".git")),
            ".git must be skipped: {rels:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_memory_views_match_disk_walk() {
        // The cached full tree derives the same collapsed/filtered views in memory that a fresh
        // disk walk produces — this is the perf path the explorer actually renders from.
        let dir = scratch("cache");
        let full = full_rows(&dir);
        let collapsed = top_level_dirs(&dir);
        assert_eq!(
            collapse_view(&full, &collapsed),
            build_rows(&dir, &collapsed),
            "in-memory collapse == disk walk"
        );
        assert_eq!(
            filter_view(&full, "sim"),
            filter_rows(&dir, &collapsed, "sim"),
            "in-memory filter == disk filter"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_finds_matches_across_collapsed_folders_with_ancestors() {
        let dir = scratch("filter");
        // Everything collapsed — the filter must still search the whole tree.
        let collapsed = top_level_dirs(&dir);
        let rows = filter_rows(&dir, &collapsed, "sim");
        let rels: Vec<&str> = rows.iter().map(|r| r.rel.as_str()).collect();
        // The match plus its ancestor dirs are kept; unrelated files are dropped.
        assert!(
            rels.contains(&"crates/city/src/sim.rs"),
            "match kept: {rels:?}"
        );
        assert!(rels.contains(&"crates"), "ancestor kept: {rels:?}");
        assert!(rels.contains(&"crates/city/src"), "ancestor kept: {rels:?}");
        assert!(
            !rels.contains(&"crates/city/src/main.rs"),
            "non-match dropped: {rels:?}"
        );
        assert!(!rels.contains(&"Cargo.toml"), "non-match dropped: {rels:?}");
        // An empty query falls back to the normal collapsed view.
        let normal = filter_rows(&dir, &collapsed, "  ");
        assert_eq!(
            normal,
            build_rows(&dir, &collapsed),
            "blank query = normal tree"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_level_dirs_are_the_non_noise_folders_and_compact_the_tree() {
        let dir = scratch("compact");
        let top = top_level_dirs(&dir);
        // Only the real top-level folder; noise (target/.git) and files excluded.
        assert!(top.contains("crates"), "{top:?}");
        assert!(!top.contains("target"), "noise excluded: {top:?}");
        assert!(!top.contains(".git"), "noise excluded: {top:?}");
        assert!(!top.contains("Cargo.toml"), "files excluded: {top:?}");
        // Seeding it collapses the tree: `crates` shows but its children don't.
        let rows = build_rows(&dir, &top);
        let rels: Vec<&str> = rows.iter().map(|r| r.rel.as_str()).collect();
        assert!(rels.contains(&"crates"), "folder header shows: {rels:?}");
        assert!(
            !rels.iter().any(|r| r.starts_with("crates/")),
            "collapsed folder hides children: {rels:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirs_come_before_files_at_each_level() {
        let dir = scratch("order");
        let rows = build_rows(&dir, &HashSet::new());
        // At the root: `crates` (dir) must appear before `Cargo.toml`/`README.md` (files).
        let crates_i = rows.iter().position(|r| r.rel == "crates").unwrap();
        let cargo_i = rows.iter().position(|r| r.rel == "Cargo.toml").unwrap();
        assert!(crates_i < cargo_i, "dir before file at root: {rows:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collapsed_dir_hides_its_children() {
        let dir = scratch("collapse");
        let mut collapsed = HashSet::new();
        collapsed.insert("crates".to_string());
        let rows = build_rows(&dir, &collapsed);
        let rels: Vec<&str> = rows.iter().map(|r| r.rel.as_str()).collect();
        assert!(rels.contains(&"crates"), "the dir row itself still shows");
        assert!(
            !rels.iter().any(|r| r.starts_with("crates/")),
            "children of a collapsed dir are hidden: {rels:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn depth_reflects_nesting() {
        let dir = scratch("depth");
        let rows = build_rows(&dir, &HashSet::new());
        let root_file = rows.iter().find(|r| r.rel == "Cargo.toml").unwrap();
        assert_eq!(root_file.depth, 0);
        let nested = rows
            .iter()
            .find(|r| r.rel == "crates/city/src/main.rs")
            .unwrap();
        assert_eq!(nested.depth, 3, "crates/city/src/main.rs is 3 deep");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
