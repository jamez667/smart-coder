//! Fixtures for this crate's own tests. No model, no network — every test here
//! is pure logic over strings or a scratch directory (spec 11).

use std::path::PathBuf;

/// The real workspace root.
///
/// `CARGO_MANIFEST_DIR` is `<root>/crates/sc-trace`, so the root is two levels
/// up. Several tests run against the actual repo — most importantly the one
/// asserting no anchor in `docs/specs/` is broken, which is the check protecting
/// the checker itself.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/sc-trace has a grandparent")
        .to_path_buf()
}

/// A scratch directory for tests that need a repo on disk. Dependency-free
/// (pid + nanos), matching the rest of the workspace rather than pulling in
/// `tempfile` for test-only use.
pub fn temp_repo(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-trace-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Write `contents` to `rel` under `root`, creating parent directories.
pub fn write(root: &std::path::Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, contents).unwrap();
}

/// A minimal workspace manifest listing `members`.
pub fn workspace_manifest(members: &[&str]) -> String {
    let list = members
        .iter()
        .map(|m| format!("    \"crates/{m}\","))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[workspace]\nresolver = \"2\"\nmembers = [\n{list}\n]\n")
}

/// A minimal member manifest for `name`.
pub fn crate_manifest(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n")
}
