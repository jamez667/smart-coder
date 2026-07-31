//! Fixtures for this crate's own tests.
//!
//! Everything here is host-runnable with no model and no network (spec 11). Note
//! that tests build **their own scratch repositories** and never touch the
//! workspace they run in — the daemon serves any repo, and a test that leaned on
//! this one would quietly encode the opposite.

use std::path::{Path, PathBuf};

/// A scratch directory. Dependency-free (pid + nanos), matching the rest of the
/// workspace rather than pulling in `tempfile` for test-only use.
pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-daemon-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A scratch directory that looks like a git working tree.
///
/// Only a `.git/` directory — enough for the preflight checks, which read marker
/// files rather than shelling out to git.
pub fn temp_repo(tag: &str) -> PathBuf {
    let d = temp_dir(tag);
    std::fs::create_dir_all(d.join(".git")).unwrap();
    d
}

/// Mark a repo as mid-operation, the way git does.
pub fn interrupt(repo: &Path, marker: &str) {
    let p = repo.join(".git").join(marker);
    if marker.contains("rebase") {
        std::fs::create_dir_all(&p).unwrap();
    } else {
        std::fs::write(&p, "interrupted\n").unwrap();
    }
}
