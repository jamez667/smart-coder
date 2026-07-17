//! Small filesystem helpers for the harness: recursive copy, content hashing,
//! and self-cleaning temporary workspaces. Dependency-free on purpose.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Recursively copy `src` into `dst` (creating `dst` if needed).
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Hash the contents of a file. `None` if the file does not exist, so callers can
/// detect deletion as a change (spec 11 — frozen contract tests).
pub fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temporary directory that removes itself on drop.
pub struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    /// Create a fresh, uniquely-named temp directory under the system temp dir.
    pub fn new(tag: &str) -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let safe_tag: String = tag
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let path = std::env::temp_dir().join(format!(
            "sc-eval-{safe_tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        // Best-effort cleanup; ignore errors so drop never panics.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
