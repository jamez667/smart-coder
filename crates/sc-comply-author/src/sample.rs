//! A sample workspace to test a pack's globs and paths against.
//!
//! Several lints can only be decided against real files: whether a glob selects
//! anything, whether a `json-path` target is usually absent, whether a
//! `must-not-match` pattern hits the pack's own sources. Those need a concrete
//! tree.
//!
//! The sample is *representative*, not authoritative — a glob matching nothing
//! here means "inert against this repo", not "inert everywhere". The lint
//! wording reflects that, and a report always states which sample was used.

use std::path::{Path, PathBuf};

use sc_comply::scan::{scan_workspace, TextFile};

/// A scanned workspace the lints can interrogate.
pub struct Sample {
    /// Absolute path, for the report header.
    pub root: PathBuf,
    files: Vec<TextFile>,
}

impl Sample {
    /// Scan `root`, reusing sc-comply's own walker so the lints see exactly the
    /// files an audit would.
    pub fn load(root: &Path) -> Self {
        Sample {
            root: root.to_path_buf(),
            files: scan_workspace(root),
        }
    }

    /// Build from an explicit file list, for tests.
    pub fn from_files(root: impl Into<PathBuf>, files: Vec<TextFile>) -> Self {
        Sample {
            root: root.into(),
            files,
        }
    }

    pub fn files(&self) -> &[TextFile] {
        &self.files
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Workspace-relative paths matching `glob`.
    pub fn matching(&self, glob: &sc_comply::Glob) -> Vec<&TextFile> {
        self.files
            .iter()
            .filter(|f| glob.is_match(&f.path))
            .collect()
    }

    /// Does a literal workspace-relative path exist in the sample?
    ///
    /// Checks the scanned text files first, then falls back to the filesystem so
    /// directories (which the scan lists only via their contents) still count —
    /// `.github/workflows` is a directory and must not read as absent.
    pub fn has_path(&self, rel: &str) -> bool {
        if self.files.iter().any(|f| f.path == rel) {
            return true;
        }
        self.root.join(rel).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> TextFile {
        TextFile {
            path: path.to_string(),
            contents: String::new(),
            ignored: false,
        }
    }

    fn sample() -> Sample {
        Sample::from_files(
            "/ws",
            vec![
                file("src/lib.rs"),
                file("src/main.rs"),
                file(".github/workflows/ci.yml"),
                file("Cargo.toml"),
            ],
        )
    }

    #[test]
    fn matching_applies_the_glob() {
        let s = sample();
        let g = sc_comply::Glob::new("**/*.rs").expect("glob");
        let hits: Vec<&str> = s.matching(&g).iter().map(|f| f.path.as_str()).collect();
        assert_eq!(hits, vec!["src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn matching_can_be_empty() {
        let s = sample();
        let g = sc_comply::Glob::new("**/*.tf").expect("glob");
        assert!(s.matching(&g).is_empty());
    }

    #[test]
    fn has_path_finds_scanned_files() {
        let s = sample();
        assert!(s.has_path("Cargo.toml"));
        assert!(!s.has_path("nope.toml"));
    }

    #[test]
    fn an_empty_sample_reports_itself_empty() {
        let s = Sample::from_files("/ws", vec![]);
        assert!(s.is_empty());
    }
}
