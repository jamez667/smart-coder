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
            | ".dumb-coder"
            | ".pytest_cache"
            | "screenshots"
            | "dist"
            | "build"
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
        let dir = std::env::temp_dir().join(format!("dc-win-ft-{}-{}", tag, std::process::id()));
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
