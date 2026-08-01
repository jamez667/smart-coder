//! Writing a file so a reader never sees it half-written.
//!
//! The same temp → fsync → rename discipline `sc-workflow` uses for `state.json`,
//! and for the same reason: a plain `fs::write` truncates first, so a crash or a
//! full disk mid-write leaves a truncated file. For the queue that would mean a
//! task record that no longer parses — and a task the daemon can no longer see is
//! a task the developer filed and lost.
//!
//! Kept here rather than shared from `sc-workflow` because it is nine lines and a
//! dependency edge is a worse trade; `sc-comply` set the same precedent with its
//! own walker.

use std::path::Path;

use sc_proto::Result;

/// Write `bytes` to `path` atomically.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    write_inner(path, bytes, false)
}

/// Write atomically, readable only by the owner.
///
/// For files holding a secret — the daemon's config carries the server API key.
/// The permissions are set on the **temp file, before the rename**, so the secret
/// is never briefly world-readable: setting them afterwards leaves a window in
/// which another user can open it, and an attacker who loses that race still only
/// has to win it once.
///
/// A no-op on Windows, where the mode bits do not apply and the inherited ACL is
/// what governs.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_inner(path, bytes, true)
}

fn write_inner(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The pid in the temp name keeps two processes from colliding on it.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        if private {
            set_owner_only(&f)?;
        }
        f.write_all(bytes)?;
        // Without this the rename can land before the contents reach disk, and a
        // power cut leaves an intact-looking file full of zeroes.
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave a temp behind to be mistaken for a real record.
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

#[cfg(unix)]
fn set_owner_only(f: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only(_f: &std::fs::File) -> Result<()> {
    // Windows has no mode bits; the file inherits the directory's ACL, and
    // `~/.smart-coder` is already under the user's profile.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    #[test]
    fn a_write_replaces_the_previous_contents_wholesale() {
        let dir = temp_dir("atomic");
        let path = dir.join("t.json");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn it_creates_missing_parent_directories() {
        let dir = temp_dir("atomic-nested");
        let path = dir.join("a").join("b").join("t.json");
        write(&path, b"x").unwrap();
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        // A stray `t.tmp1234` beside the queue would be read as a task record at
        // worst, and confuse a human at best.
        let dir = temp_dir("atomic-clean");
        write(&dir.join("t.json"), b"x").unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_private_write_is_owner_only_and_never_briefly_world_readable() {
        // The daemon's config carries the server API key. Setting the mode after
        // the rename would leave a window another user can open it in — and an
        // attacker only has to win that race once.
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("atomic-private");
        let path = dir.join("secret.json");
        write_private(&path, b"{\"key\":\"s3cret\"}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_private_write_is_still_atomic_and_leaves_no_temp() {
        // The permission handling must not have cost the durability property.
        let dir = temp_dir("atomic-private-clean");
        let path = dir.join("secret.json");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crashed_write_leaves_the_previous_file_intact() {
        // Simulate the crash: a temp file exists, the rename never happened.
        let dir = temp_dir("atomic-crash");
        let path = dir.join("t.json");
        write(&path, b"the good record").unwrap();
        std::fs::write(dir.join("t.tmp999999"), b"{ truncated").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the good record",
            "the reader sees the old file, never a partial one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
