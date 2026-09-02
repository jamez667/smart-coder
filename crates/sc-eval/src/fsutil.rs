//! Small filesystem helpers for the harness: recursive copy, content hashing,
//! and self-cleaning temporary workspaces. Dependency-free on purpose.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Recursively copy `src` into `dst` (creating `dst` if needed).
///
/// Copied files are stamped as modified NOW. `std::fs::copy` preserves the source
/// mtime, and that silently broke every `cargo`-based task: the runner verifies red
/// (which builds `target/`), copies the solution in, then verifies green -- but the
/// solution files arrived carrying the repo's older mtimes, so cargo's fingerprint
/// check judged them unchanged and reused the stale build. The result was
/// `no method named iter2_without` for a file that visibly contained it, and
/// `FileSolver` -- applying a task's own known-good answer -- scored 8/10.
///
/// Confirmed by reproduction: `cp -r` (which restamps) passes, `cp -p -r` (which
/// preserves, like `fs::copy`) fails on the identical tree.
///
/// The eight `rustc --test` rungs never hit this because they recompile
/// unconditionally, which is why it hid for so long.
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
            // Best-effort: a filesystem that refuses the stamp is not a reason to
            // fail the copy, and the worst case is the stale-build behaviour we had
            // before.
            let _ = std::fs::File::options()
                .write(true)
                .open(&to)
                .and_then(|f| f.set_modified(SystemTime::now()));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// **A copied file must look NEWER than whatever was built before it.**
    ///
    /// `std::fs::copy` preserves the source mtime, and the runner's sequence is
    /// verify-red (which builds `target/`), copy the solution in, verify-green. With
    /// the mtime preserved, the solution arrived looking OLDER than the artifacts the
    /// red check had just produced, so cargo reused the stale build and reported a
    /// method missing from a file that visibly contained it. `FileSolver` -- applying
    /// each task's own known-good answer -- scored 8/10 on a suite where every task
    /// is solvable, and the two failing rungs were the only `cargo`-based ones.
    #[test]
    fn a_copied_file_is_newer_than_a_pre_existing_build_artifact() {
        let root = std::env::temp_dir().join(format!("sc-eval-mtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        // A source file with a deliberately OLD mtime, as a repo checkout has.
        let old_file = src.join("lib.rs");
        std::fs::write(&old_file, "fn f() {}").unwrap();
        let long_ago = SystemTime::now() - std::time::Duration::from_secs(60 * 60 * 24);
        std::fs::File::options()
            .write(true)
            .open(&old_file)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        // A build artifact produced "just now", standing in for target/.
        let artifact = dst.join("artifact.bin");
        std::fs::write(&artifact, "built").unwrap();
        let artifact_mtime = std::fs::metadata(&artifact).unwrap().modified().unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        let copied = std::fs::metadata(dst.join("lib.rs"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            copied >= artifact_mtime,
            "a copied source must not look older than an existing build artifact \
             (copied {copied:?}, artifact {artifact_mtime:?}) -- cargo would reuse the \
             stale build"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The copy must still actually copy: contents and nesting preserved.
    #[test]
    fn copy_dir_recursive_preserves_contents_and_structure() {
        let root = std::env::temp_dir().join(format!("sc-eval-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("top.rs"), "top").unwrap();
        std::fs::write(src.join("nested/deep.rs"), "deep").unwrap();

        let dst = root.join("dst");
        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("top.rs")).unwrap(), "top");
        assert_eq!(
            std::fs::read_to_string(dst.join("nested/deep.rs")).unwrap(),
            "deep"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
