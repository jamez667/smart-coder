//! Shared test helpers.
//!
//! Compiled only under `cfg(test)`. Dependency-free temp directories, following
//! the `sc-index` convention rather than pulling in `tempfile`: pid plus nanos
//! keeps parallel `cargo test` runs from colliding.

use std::path::{Path, PathBuf};

/// Create a uniquely-named temp directory for a test.
///
/// Callers are expected to `let _ = std::fs::remove_dir_all(&root);` at the end
/// of the test — best-effort, matching the existing crates.
pub fn temp_repo(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("sc-comply-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// Write a file, creating parent directories as needed.
pub fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(p, body).expect("write");
}
