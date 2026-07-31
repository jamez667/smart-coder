//! Workspace helpers: the sandbox join and the filesystem source-file ledger.

use std::path::{Component, Path, PathBuf};

use sc_proto::{DcError, Result};

/// List the **source** files actually on disk under `workspace` (workspace-relative,
/// `/`-separated, sorted), excluding test files and tooling caches/deps. This is
/// filesystem ground truth — what the run has *really* built so far, independent of the
/// model's own action history — so the agent loop can show the model a progress ledger and
/// stop it re-creating files that already exist (spec 03/05). Mirrors
/// `sc_win::config::source_files`; kept in sync deliberately.
pub fn source_files(workspace: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip hidden/dot dirs (.smart-coder, .pytest_cache, .git), caches, deps.
            if name.starts_with('.') || name == "__pycache__" || name == "node_modules" {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !is_test_file(&rel) && !is_workflow_artifact(&rel) {
                        out.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Whether a path is the workflow's **own output** rather than project source.
///
/// `specs/<slug>/` holds the planning artifacts a run writes — `spec.md`,
/// `state.json`, and the daemon's `lease.json` — and unlike `.smart-coder/` it is
/// deliberately not hidden, because those artifacts are meant to be reviewed as a
/// diff and committed.
///
/// Surveying them as *source* feeds a run its own bookkeeping. Observed live: a
/// spec drafted against an empty repository listed `lease.json` under "Files to
/// Touch", because the only file the survey found was the lease the drafting run
/// was itself holding. The model was reasoning correctly about a survey that was
/// wrong.
fn is_workflow_artifact(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    if !lower.starts_with("specs/") {
        return false;
    }
    // Only the machinery — a hand-written `specs/foo/notes.md` is still source,
    // and excluding a whole directory tree would hide real design documents.
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    matches!(
        name,
        "state.json"
            | "lease.json"
            | "spec.md"
            | "architecture.md"
            | "layout.md"
            | "breakdown.md"
            | "decomposition.md"
    )
}

/// Whether a workspace-relative path looks like a test file (so it's excluded from the
/// source-file ledger — the tests are frozen, not the run's output).
fn is_test_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("test_")
        || lower.contains(".test.")
        || lower.contains("_test.")
        || lower.contains(".spec.")
}

/// Join `rel` onto `workspace`, rejecting absolute paths and `..` traversal
/// (spec 04 — sandboxed to the workspace root).
pub fn safe_join(workspace: &Path, rel: &str) -> Result<PathBuf> {
    let rp = Path::new(rel);
    if rp.is_absolute() {
        return Err(DcError::Eval(format!("absolute paths not allowed: {rel}")));
    }
    for c in rp.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(DcError::Eval(format!("path escapes workspace: {rel}"))),
        }
    }
    Ok(workspace.join(rp))
}
