//! The edit journal (spec 04 — "all edits go through a single apply-and-record
//! path so every change can be diffed, shown to the user, and rolled back").
//!
//! Each mutating tool call is recorded as a before/after snapshot of the affected
//! file. From the journal we can render a compact change summary for the user and
//! roll the workspace back to its pre-run state — the safety net that lets a small
//! model edit freely without leaving the workspace wedged.

use std::path::{Path, PathBuf};

/// One recorded mutation: the file's content before and after the call. `None`
/// means the file did not exist at that point (creation/deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRecord {
    /// Workspace-relative path.
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl EditRecord {
    /// A one-line summary of the change, with a byte delta.
    pub fn summary(&self) -> String {
        match (&self.before, &self.after) {
            (None, Some(a)) => format!("created {} (+{} bytes)", self.path, a.len()),
            (Some(b), None) => format!("deleted {} (-{} bytes)", self.path, b.len()),
            (Some(b), Some(a)) if b != a => {
                let (db, da) = (b.len() as isize, a.len() as isize);
                format!("edited {} ({:+} bytes)", self.path, da - db)
            }
            _ => format!("touched {} (no change)", self.path),
        }
    }
}

/// An ordered record of all mutations in a run.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    records: Vec<EditRecord>,
}

impl Journal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a mutation by snapshotting `path` against `before`. Call this with
    /// the pre-mutation content captured *before* the tool ran, after the tool
    /// has run, so `after` is read fresh from disk.
    pub fn record(&mut self, workspace: &Path, rel_path: &str, before: Option<String>) {
        let after = std::fs::read_to_string(join(workspace, rel_path)).ok();
        // Don't record no-ops (e.g. a rejected edit that never touched the file).
        if before != after {
            self.records.push(EditRecord {
                path: rel_path.to_string(),
                before,
                after,
            });
        }
    }

    /// Snapshot a file's current content (for capturing `before`).
    pub fn snapshot(workspace: &Path, rel_path: &str) -> Option<String> {
        std::fs::read_to_string(join(workspace, rel_path)).ok()
    }

    pub fn records(&self) -> &[EditRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// A compact multi-line summary of every change (spec 06 — diff overview).
    pub fn change_summary(&self) -> String {
        if self.records.is_empty() {
            return "no files changed".to_string();
        }
        let mut s = format!("{} change(s):", self.records.len());
        for r in &self.records {
            s.push_str(&format!("\n  {}", r.summary()));
        }
        s
    }

    /// Roll the workspace back to the pre-run state by replaying records in
    /// reverse, restoring each file's `before` (or removing created files).
    pub fn rollback(&self, workspace: &Path) -> std::io::Result<()> {
        for r in self.records.iter().rev() {
            let path = join(workspace, &r.path);
            match &r.before {
                Some(content) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, content)?;
                }
                None => {
                    // Was created during the run — remove it.
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        Ok(())
    }
}

fn join(workspace: &Path, rel: &str) -> PathBuf {
    workspace.join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-tools-journal-{tag}-{}-{}",
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
    fn records_an_edit_and_summarizes_it() {
        let ws = temp("edit");
        std::fs::write(ws.join("a.txt"), "hello").unwrap();
        let before = Journal::snapshot(&ws, "a.txt");
        std::fs::write(ws.join("a.txt"), "hello world").unwrap();

        let mut j = Journal::new();
        j.record(&ws, "a.txt", before);
        assert_eq!(j.records().len(), 1);
        assert!(
            j.change_summary().contains("edited a.txt"),
            "{}",
            j.change_summary()
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn records_a_creation() {
        let ws = temp("create");
        let before = Journal::snapshot(&ws, "new.txt"); // None
        std::fs::write(ws.join("new.txt"), "fresh").unwrap();
        let mut j = Journal::new();
        j.record(&ws, "new.txt", before);
        assert!(j.records()[0].summary().contains("created"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn no_op_is_not_recorded() {
        let ws = temp("noop");
        std::fs::write(ws.join("a.txt"), "same").unwrap();
        let before = Journal::snapshot(&ws, "a.txt");
        // No write happens between snapshot and record.
        let mut j = Journal::new();
        j.record(&ws, "a.txt", before);
        assert!(j.is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn rollback_restores_edits_and_removes_creations() {
        let ws = temp("rollback");
        std::fs::write(ws.join("keep.txt"), "v1").unwrap();
        let mut j = Journal::new();

        // Edit keep.txt.
        let b1 = Journal::snapshot(&ws, "keep.txt");
        std::fs::write(ws.join("keep.txt"), "v2").unwrap();
        j.record(&ws, "keep.txt", b1);

        // Create new.txt.
        let b2 = Journal::snapshot(&ws, "new.txt");
        std::fs::write(ws.join("new.txt"), "created").unwrap();
        j.record(&ws, "new.txt", b2);

        j.rollback(&ws).unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("keep.txt")).unwrap(), "v1");
        assert!(!ws.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
