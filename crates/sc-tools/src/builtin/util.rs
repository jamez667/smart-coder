//! Workspace helpers: the sandbox join and the filesystem source-file ledger.

use std::path::{Component, Path, PathBuf};

use sc_proto::{DcError, Result};

/// List the **source** files actually on disk under `workspace` (workspace-relative,
/// `/`-separated, sorted), excluding test files and tooling caches/deps. This is
/// filesystem ground truth — what the run has *really* built so far, independent of the
/// model's own action history — so the agent loop can show the model a progress ledger and
/// stop it re-creating files that already exist (spec 03/05).
///
/// The directory policy is [`sc_index::walk`]'s, shared with every other walk in the
/// project (spec 23) — including skipping BUILD OUTPUT, which several of the old
/// walks were missing. Measured on a real Rust project: 40,585 of 41,180 "source"
/// files were build artifacts, 98.5% noise burying 595 real files. What stays *here*
/// is the policy that is genuinely this ledger's own: tests are frozen, not output,
/// and the workflow's own artifacts are not project source.
///
/// `sc_win::config::workspace::source_files` used to be a hand-synced copy of this
/// and now calls it.
pub fn source_files(workspace: &Path) -> Vec<String> {
    sc_index::walk(workspace, &sc_index::WalkOptions::default())
        .into_iter()
        .map(|f| f.rel)
        .filter(|rel| !is_test_file(rel) && !is_workflow_artifact(rel))
        .collect()
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

#[cfg(test)]
mod build_output_is_not_source {
    use super::*;

    /// **`target/` is not source.**
    ///
    /// It was walked like any other directory, and it dominates: measured on a real Rust
    /// project, 40,585 of 41,180 files were build artifacts. Any consumer that caps this
    /// list truncated before reaching real code, and any consumer that shows it to a model
    /// was mostly showing build stamps.
    #[test]
    fn build_directories_are_skipped() {
        let dir = std::env::temp_dir().join(format!("sc-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug/build")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("target/debug/build/stamp.rs"), "// generated").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "//dep").unwrap();

        let files = source_files(&dir);
        assert!(
            files.iter().any(|f| f == "src/main.rs"),
            "real source must survive, got {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.starts_with("target/")),
            "build output must be skipped, got {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.starts_with("node_modules/")),
            "deps must be skipped, got {files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
