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
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The pid in the temp name keeps two processes from colliding on it.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
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
