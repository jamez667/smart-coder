//! Resolving an anchor against the workspace: does what it names exist, and does
//! what it asserts hold?
//!
//! The shape is `sc-comply`'s `Observation`, and for the same reason: a resolver
//! must be able to say **"I could not determine this"** as a first-class answer,
//! distinct from "it is not there". Collapsing the two is how a checker starts
//! lying quietly — one is a fact about the code, the other a fact about the
//! checker (spec 17).

pub mod cardinality;
pub mod path;
pub mod symbol;

use std::path::Path;

use crate::anchor::{Anchor, AnchorKind};
use crate::manifest::Workspace;
use crate::status::ClaimStatus;

/// Where a resolved anchor actually landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    /// 1-based line, when the target is a symbol rather than a whole file.
    pub line: Option<usize>,
}

impl Located {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line: None,
        }
    }

    pub fn at(path: impl Into<String>, line: usize) -> Self {
        Self {
            path: path.into(),
            line: Some(line),
        }
    }

    /// `path:line`, for the report's location column.
    pub fn display(&self) -> String {
        match self.line {
            Some(line) => format!("{}:{}", self.path, line),
            None => self.path.clone(),
        }
    }
}

/// What a resolver saw.
///
/// `status` is the verdict; `note` explains it whenever the verdict is anything
/// other than plain `Ok`, and is **mandatory** for `Unknown` — an undeterminable
/// result with no stated reason is indistinguishable from a bug in the checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub status: ClaimStatus,
    pub targets: Vec<Located>,
    pub note: Option<String>,
}

impl Resolution {
    /// Resolved, and any assertion holds.
    pub fn ok(targets: Vec<Located>) -> Self {
        Self {
            status: ClaimStatus::Ok,
            targets,
            note: None,
        }
    }

    /// The anchor names something that is not there.
    pub fn broken(note: impl Into<String>) -> Self {
        Self {
            status: ClaimStatus::Broken,
            targets: Vec::new(),
            note: Some(note.into()),
        }
    }

    /// It resolved, but the assertion is false.
    pub fn stale(targets: Vec<Located>, note: impl Into<String>) -> Self {
        Self {
            status: ClaimStatus::Stale,
            targets,
            note: Some(note.into()),
        }
    }

    /// The checker could not determine this. The reason is required.
    pub fn unknown(note: impl Into<String>) -> Self {
        Self {
            status: ClaimStatus::Unknown,
            targets: Vec::new(),
            note: Some(note.into()),
        }
    }

    /// Undeterminable, but we know where the candidates are.
    pub fn unknown_at(targets: Vec<Located>, note: impl Into<String>) -> Self {
        Self {
            status: ClaimStatus::Unknown,
            targets,
            note: Some(note.into()),
        }
    }
}

/// Resolve one anchor.
pub fn resolve(anchor: &Anchor, root: &Path, ws: &Workspace) -> Resolution {
    match &anchor.kind {
        AnchorKind::Path { path } => path::resolve(path, root),
        AnchorKind::Symbol { sym } => symbol::resolve(sym, root, ws, None),
        AnchorKind::SymbolLen { sym, expect } => symbol::resolve(sym, root, ws, Some(*expect)),
        // A malformed anchor is retained and reported, never dropped: an anchor
        // that vanishes on a typo leaves the spec reading as governed while
        // nothing verifies it.
        AnchorKind::Malformed { why } => Resolution::unknown(format!("malformed anchor: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::parse_anchors;
    use crate::test_support::{repo_root, temp_repo};

    #[test]
    fn a_malformed_anchor_resolves_to_unknown_with_a_reason() {
        let anchors = parse_anchors("s.md", "<!--@ justaword -->");
        let root = temp_repo("malformed");
        let r = resolve(&anchors[0], &root, &Workspace::default());
        assert_eq!(r.status, ClaimStatus::Unknown);
        // Never Broken: an unreadable anchor says nothing about the code.
        assert!(r.note.unwrap().contains("malformed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn located_renders_with_and_without_a_line() {
        assert_eq!(Located::file("a/b.rs").display(), "a/b.rs");
        assert_eq!(Located::at("a/b.rs", 12).display(), "a/b.rs:12");
    }

    #[test]
    fn every_non_ok_resolution_carries_a_reason() {
        // A verdict a human cannot act on is a verdict that gets ignored.
        let root = repo_root();
        let ws = Workspace::load(&root);
        for text in [
            "<!--@ crates/does-not-exist.rs -->",
            "<!--@ sc_ghost::thing -->",
            "<!--@ sc_workflow::Phase::ALL len=99 -->",
            "<!--@ oops -->",
        ] {
            let a = parse_anchors("s.md", text);
            let r = resolve(&a[0], &root, &ws);
            assert_ne!(r.status, ClaimStatus::Ok, "{text}");
            assert!(r.note.is_some(), "{text} gave no reason");
        }
    }
}
