//! Workspace-level retrieval: walk a repo, index it, and answer the agent's
//! retrieval tools (`find_symbol`) and the Context Manager's repo-map request.
//!
//! Reads are confined to the workspace and skip the usual noise directories. A
//! file that can't be read or isn't a supported language is simply skipped — one
//! bad file never breaks retrieval.

use std::path::Path;

use crate::repomap::{build_repo_map, render_repo_map, Boosts, SourceFile};
use crate::symbols::{extract_symbols, Language};

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__"];

/// Collect every supported source file under `root` as `(relative path, source)`.
pub fn collect_sources(root: &Path) -> Vec<SourceFile> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if Language::from_path(&rel).is_none() {
                    continue;
                }
                if let Ok(source) = std::fs::read_to_string(&path) {
                    out.push(SourceFile { path: rel, source });
                }
            }
        }
    }
    // Stable order so the index (and any ties) are deterministic.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Build a token-budgeted PageRank repo map over the workspace, with boosts for
/// symbols mentioned in the task and files already in play (spec 05).
pub fn repo_map(root: &Path, boosts: &Boosts, top_k: usize) -> String {
    let files = collect_sources(root);
    let ranked = build_repo_map(&files, boosts, top_k);
    render_repo_map(&ranked)
}

/// Locate where `name` is defined across the workspace. Returns a `find_symbol`
/// observation: each definition as `path:line`, or a clear "not found".
pub fn find_symbol(root: &Path, name: &str) -> String {
    if name.is_empty() {
        return "find_symbol: empty name".to_string();
    }
    let mut hits = Vec::new();
    for f in collect_sources(root) {
        let lang = match Language::from_path(&f.path) {
            Some(l) => l,
            None => continue,
        };
        for d in extract_symbols(lang, &f.source).defs {
            if d.name == name {
                hits.push(format!("{}:{}", f.path, d.line));
            }
        }
    }
    if hits.is_empty() {
        format!("find_symbol {name:?}: no definition found")
    } else {
        hits.sort();
        format!("find_symbol {name:?}: {}", hits.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-index-ws-{tag}-{}-{}",
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
    fn collect_skips_noise_dirs_and_non_source() {
        let root = temp_repo("collect");
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(root.join("notes.md"), "# hi").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/b.rs"), "fn b() {}").unwrap();

        let files = collect_sources(&root);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs"]); // md skipped, target/ skipped
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_symbol_locates_a_definition() {
        let root = temp_repo("find");
        std::fs::write(root.join("a.rs"), "fn one() {}\nfn target() {}\n").unwrap();
        let out = find_symbol(&root, "target");
        assert!(out.contains("a.rs:2"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_symbol_reports_not_found() {
        let root = temp_repo("notfound");
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        assert!(find_symbol(&root, "ghost").contains("no definition found"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repo_map_over_workspace_ranks_central_symbols() {
        let root = temp_repo("map");
        std::fs::write(root.join("core.rs"), "pub fn core() {}").unwrap();
        std::fs::write(root.join("a.rs"), "fn a() { core(); }").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() { core(); }").unwrap();
        let map = repo_map(&root, &Boosts::default(), 10);
        assert!(map.contains("core.rs:1  core"), "{map}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
