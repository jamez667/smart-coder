//! A swarm worker (spec 08 — "each worker IS a `dumb-coder` agent loop").
//!
//! A worker runs the unchanged `dc_core` agent loop against a **scratch copy** of
//! the workspace, scoped to one subtask. It never touches the real workspace;
//! instead it returns the set of file changes it *proposes* (a [`ProposedChange`]
//! per file). The orchestrator later applies accepted proposals to the real
//! workspace one at a time (serialized writes, spec 08).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dc_core::{run_agent_recovering, AgentConfig, AgentReport, ParseRepair};
use dc_model::ModelBackend;
use dc_tools::default_registry;

use crate::board::Subtask;

/// One file the worker proposes to change. `after == None` means delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedChange {
    pub path: String,
    pub after: Option<String>,
}

/// The outcome of one worker running one subtask.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    pub subtask_id: String,
    /// Whether the worker's own loop reported success (finished / verified).
    pub finished: bool,
    pub verified: Option<bool>,
    /// File changes the worker proposes, relative to the workspace it was given.
    pub changes: Vec<ProposedChange>,
    pub report_summary: String,
}

impl WorkerResult {
    pub fn made_changes(&self) -> bool {
        !self.changes.is_empty()
    }
}

/// Run `subtask` on a worker `backend` (optionally with an `advisor`), against a
/// scratch copy of `workspace`. Returns the proposed changes without modifying
/// the real workspace.
pub fn run_worker(
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    subtask: &Subtask,
    workspace: &Path,
    cfg: &AgentConfig,
) -> WorkerResult {
    // Scratch copy: an isolated workspace the worker can freely edit.
    let scratch = match scratch_copy(workspace, &subtask.id) {
        Ok(p) => p,
        Err(e) => {
            return WorkerResult {
                subtask_id: subtask.id.clone(),
                finished: false,
                verified: None,
                changes: Vec::new(),
                report_summary: format!("worker setup failed: {e}"),
            }
        }
    };

    let before = snapshot_tree(&scratch);

    let instruction = worker_instruction(subtask, &scratch);
    let registry = default_registry();
    let report: Option<AgentReport> = run_agent_recovering(
        backend,
        advisor,
        &registry,
        &ParseRepair,
        &instruction,
        &scratch,
        cfg,
    )
    .ok();

    let after = snapshot_tree(&scratch);
    let changes = diff_trees(&before, &after);

    let (finished, verified, summary) = match &report {
        Some(r) => (
            r.finished,
            r.verified,
            format!("{:?}, {} change(s)", r.stop_reason, changes.len()),
        ),
        None => (false, None, "worker errored".to_string()),
    };

    // Best-effort cleanup of the scratch dir.
    let _ = std::fs::remove_dir_all(&scratch);

    WorkerResult {
        subtask_id: subtask.id.clone(),
        finished,
        verified,
        changes,
        report_summary: summary,
    }
}

/// The tight, single-purpose instruction handed to a worker (spec 08 — scoped).
///
/// When the subtask names files, we inline their *current contents* so a tiny
/// worker can write a correct anchored edit without first spending turns reading
/// (and without guessing `old_str` and looping on "0 matches"). Tests files are
/// included as read-only context but explicitly off-limits to edit.
fn worker_instruction(subtask: &Subtask, scratch: &Path) -> String {
    let mut s = format!("Your subtask: {}", subtask.goal);
    if !subtask.files.is_empty() {
        s.push_str("\n\nRelevant files (current contents — edit_file against these exactly):");
        for f in &subtask.files {
            if let Ok(content) = std::fs::read_to_string(scratch.join(f)) {
                // Keep it bounded so a huge file doesn't blow the worker's window.
                let body: String = content.chars().take(2000).collect();
                s.push_str(&format!("\n\n=== {f} ===\n{body}"));
            }
        }
    }
    s.push_str(
        "\n\nMake only the change this subtask needs, with edit_file using an \
         exact old_str from the contents above. Do not modify test files. Then \
         run_verification.",
    );
    s
}

/// Copy `workspace` into a fresh scratch directory the worker owns.
fn scratch_copy(workspace: &Path, tag: &str) -> std::io::Result<PathBuf> {
    let dst = std::env::temp_dir().join(format!(
        "dc-swarm-{}-{}-{}",
        sanitize(tag),
        std::process::id(),
        unique()
    ));
    copy_dir(workspace, &dst)?;
    Ok(dst)
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        // Skip noise that would bloat the copy / break isolation.
        if matches!(
            name.to_string_lossy().as_ref(),
            ".git" | "target" | "node_modules" | "__pycache__" | ".venv" | ".pytest_cache"
        ) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Snapshot every UTF-8 text file under `root` as `relpath -> contents`.
fn snapshot_tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let n = entry.file_name().to_string_lossy().into_owned();
                if !matches!(
                    n.as_str(),
                    ".git" | "target" | "node_modules" | "__pycache__" | ".pytest_cache" | ".venv"
                ) {
                    stack.push(path);
                }
            } else if let Ok(content) = std::fs::read_to_string(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, content);
            }
        }
    }
    out
}

/// Files that differ between `before` and `after`: created, edited, or deleted.
fn diff_trees(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<ProposedChange> {
    let mut changes = Vec::new();
    for (path, content) in after {
        if before.get(path) != Some(content) {
            changes.push(ProposedChange {
                path: path.clone(),
                after: Some(content.clone()),
            });
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changes.push(ProposedChange {
                path: path.clone(),
                after: None, // deleted
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc-swarm-wt-{tag}-{}", unique()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn diff_detects_edit_create_delete() {
        let mut before = BTreeMap::new();
        before.insert("keep.txt".to_string(), "same".to_string());
        before.insert("edit.txt".to_string(), "v1".to_string());
        before.insert("gone.txt".to_string(), "bye".to_string());
        let mut after = BTreeMap::new();
        after.insert("keep.txt".to_string(), "same".to_string());
        after.insert("edit.txt".to_string(), "v2".to_string());
        after.insert("new.txt".to_string(), "hi".to_string());

        let changes = diff_trees(&before, &after);
        let by: BTreeMap<_, _> = changes
            .iter()
            .map(|c| (c.path.as_str(), &c.after))
            .collect();
        assert!(!by.contains_key("keep.txt")); // unchanged
        assert_eq!(by["edit.txt"], &Some("v2".to_string()));
        assert_eq!(by["new.txt"], &Some("hi".to_string()));
        assert_eq!(by["gone.txt"], &None); // deleted
    }

    #[test]
    fn scratch_copy_is_isolated_and_snapshots() {
        let ws = temp("iso");
        std::fs::write(ws.join("a.txt"), "original").unwrap();
        let scratch = scratch_copy(&ws, "s1").unwrap();
        // Edit the scratch; the real workspace is untouched.
        std::fs::write(scratch.join("a.txt"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "original"
        );
        let snap = snapshot_tree(&scratch);
        assert_eq!(snap.get("a.txt").map(String::as_str), Some("changed"));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn worker_runs_the_loop_and_returns_a_proposed_change() {
        use dc_model::MockBackend;
        use serde_json::json;

        let ws = temp("run");
        std::fs::write(ws.join("impl.txt"), "old").unwrap();
        // Scripted worker: write a file, then finish.
        let backend = MockBackend::new([
            json!({"tool":"write_file","path":"impl.txt","content":"new"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);
        let subtask = Subtask::new("t1", "change impl.txt to new");
        let result = run_worker(&backend, None, &subtask, &ws, &AgentConfig::default());

        assert!(result.finished);
        assert!(result.made_changes());
        let change = result
            .changes
            .iter()
            .find(|c| c.path == "impl.txt")
            .unwrap();
        assert_eq!(change.after, Some("new".to_string()));
        // The REAL workspace is unchanged — the worker only proposed.
        assert_eq!(std::fs::read_to_string(ws.join("impl.txt")).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
