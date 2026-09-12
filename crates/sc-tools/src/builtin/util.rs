//! Workspace helpers: the sandbox join and the filesystem source-file ledger.

use std::path::{Path, PathBuf};

use sc_proto::{DcError, Result};

/// List the **source** files actually on disk under `workspace` (workspace-relative,
/// `/`-separated), excluding test files, tooling caches/deps, and the workflow's own
/// artifacts. This is filesystem ground truth — what the run has *really* built so far,
/// independent of the model's own action history — so the agent loop can show the model
/// a progress ledger and stop it re-creating files that already exist (spec 03/05).
///
/// The directory policy is [`sc_index::walk`]'s, shared with every other walk in the
/// project (spec 23) — including skipping BUILD OUTPUT, which several of the old walks
/// were missing. Measured on a real Rust project: 40,585 of 41,180 "source" files were
/// build artifacts, 98.5% noise burying 595 real files.
///
/// Re-exported from [`sc_fsutil`], which is now the one definition. It moved to a leaf
/// crate so the editor-only Crafter build can share it: the Crafter's guarantee is that
/// no model code appears in its dependency tree at all (spec 21), and this crate is
/// model code. The ledger's own policy — tests are frozen rather than output, and the
/// workflow's artifacts are not project source — moved with it.
pub use sc_fsutil::source_files;

/// Render `lines` with 1-based line numbers starting at `first`, one `N: text` per
/// line. This is the ONE format every tool uses to show file content, so a number the
/// model reads in a file view is the number it can hand to a line-addressed edit.
pub(super) fn number_lines(lines: &[&str], first: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {l}", first + i))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether text uses Windows line endings. Checked on the RAW bytes of a file before
/// any normalisation, so an editor can write back the endings it found.
pub(super) fn uses_crlf(raw: &str) -> bool {
    raw.contains("\r\n")
}

/// Normalise every line ending to LF, so matching and splicing work in one dialect.
pub(super) fn to_lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Convert LF text back to the file's own endings: CRLF when `crlf`, else unchanged.
/// Every editor that read a CRLF file writes CRLF back, so an edit never flips a
/// file's endings and shows up as a whole-file diff.
pub(super) fn from_lf(lf: &str, crlf: bool) -> String {
    if crlf {
        lf.replace('\n', "\r\n")
    } else {
        lf.to_string()
    }
}

/// Join `rel` onto `workspace`, rejecting absolute paths and `..` traversal
/// (spec 04 — sandboxed to the workspace root).
///
/// The rule itself is [`sc_fsutil::safe_join`]; this wraps it to add this crate's error
/// type. It moved to a leaf when the plugin host needed the same containment for a
/// plugin's path arguments (spec 25) and could not depend on this crate — the same
/// reason `is_noise_dir` and `source_files` moved there.
pub fn safe_join(workspace: &Path, rel: &str) -> Result<PathBuf> {
    sc_fsutil::safe_join(workspace, rel)
        .ok_or_else(|| DcError::Eval(format!("path escapes workspace: {rel}")))
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
