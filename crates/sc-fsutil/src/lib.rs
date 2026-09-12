//! Filesystem conventions shared by **both products** (spec 21).
//!
//! Smart Coder ships as two executables built from one workspace: the agent app and
//! the Crafter, an editor that does not depend on a single model crate. That split is
//! structural — `cargo tree -p sc-crafter` names no model code — which means anything
//! the editor needs cannot live in `sc-iterate` or `sc-tools`.
//!
//! These two helpers are exactly that case. Both were de-duplicated *into* agent crates
//! before the split (the doc comments on the old re-exports explain why, at length, and
//! they were right to). Pushing them down here keeps the de-duplication while letting
//! the editor reach them: one definition, two consumers, no model dependency.
//!
//! The dependency floor is deliberate. This crate depends on `sc-index` and nothing
//! else, and adding anything that reaches a network or a model would silently hand the
//! Crafter a transitive path to one.

use std::path::Path;

/// Directories excluded from a walk: VCS, build output, tooling caches, dependencies,
/// and generated-asset folders.
///
/// **One list, two consumers with different stakes.** For the agent this feeds prompt
/// text — the explorer, the workspace overview, and the repo overview `sc-iterate`
/// builds for the remote server — so a divergence (someone adds `.venv` to one copy)
/// silently changes agent behaviour between desktop and server. For the Crafter it is
/// what the file tree shows. A second copy has already drifted once here; it was
/// byte-identical down to the same fourteen-name match arm, and the extraction had been
/// done with the original never deleted.
///
/// The trailing dotfile rule is what makes this more than a list: any name starting
/// with `.` is noise (`.venv`, `.idea`, `.pytest_cache` all covered without an entry),
/// except `.` itself, which is the walk's own starting point.
pub fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | ".git"
            | "node_modules"
            | "__pycache__"
            | ".smart-coder"
            | ".pytest_cache"
            | "screenshots"
            | "dist"
            | "build"
            | "Library"
            | "Temp"
            | "obj"
            | "Logs"
            | "UserSettings"
            | "Builds"
    ) || name.starts_with('.') && name != "."
}

/// List the **source** files in `workspace` (workspace-relative), excluding tests and
/// the workflow's own artifacts.
///
/// For the agent this is "what a run actually built", so the UI can say "5 files built"
/// plainly. For the Crafter it is the project survey. Both want the same answer: real
/// project source, not bookkeeping.
pub fn source_files(workspace: &Path) -> Vec<String> {
    sc_index::walk(workspace, &sc_index::WalkOptions::default())
        .into_iter()
        .map(|f| f.rel)
        .filter(|rel| !is_test_file(rel) && !is_workflow_artifact(rel))
        .collect()
}

/// Whether a path is the workflow's **own output** rather than project source.
///
/// `specs/<slug>/` holds the planning artifacts a run writes — `spec.md`, `state.json`,
/// and the daemon's `lease.json` — and unlike `.smart-coder/` it is deliberately not
/// hidden, because those artifacts are meant to be reviewed as a diff and committed.
///
/// Surveying them as *source* feeds a run its own bookkeeping. Observed live: a spec
/// drafted against an empty repository listed `lease.json` under "Files to Touch",
/// because the only file the survey found was the lease the drafting run was itself
/// holding. The model was reasoning correctly about a survey that was wrong.
fn is_workflow_artifact(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    if !lower.starts_with("specs/") {
        return false;
    }
    // Only the machinery — a hand-written `specs/foo/notes.md` is still source, and
    // excluding a whole directory tree would hide real design documents.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The fourteen named directories, pinned. This list feeds prompt text on the agent
    /// side, so a silent addition or removal changes model behaviour.
    #[test]
    fn the_named_noise_dirs_are_skipped() {
        for name in [
            "target",
            ".git",
            "node_modules",
            "__pycache__",
            ".smart-coder",
            ".pytest_cache",
            "screenshots",
            "dist",
            "build",
            "Library",
            "Temp",
            "obj",
            "Logs",
            "UserSettings",
            "Builds",
        ] {
            assert!(is_noise_dir(name), "{name} must be noise");
        }
    }

    /// Any dotfile directory is noise without needing an entry — that rule is why the
    /// list does not have to name `.venv`, `.idea` or `.cargo`.
    #[test]
    fn dot_directories_are_noise_without_being_listed() {
        for name in [".venv", ".idea", ".cargo", ".github"] {
            assert!(is_noise_dir(name), "{name} must be noise");
        }
    }

    /// `.` is the walk's own starting point, not a directory to skip — the one
    /// exception the dotfile rule has to carve out.
    #[test]
    fn the_current_dir_is_not_noise() {
        assert!(!is_noise_dir("."));
    }

    #[test]
    fn ordinary_source_dirs_survive() {
        for name in ["src", "crates", "docs", "assets", "tests"] {
            assert!(!is_noise_dir(name), "{name} must not be noise");
        }
    }

    /// The workflow's own bookkeeping is not project source — the `lease.json` case
    /// that was observed feeding a run its own lease.
    #[test]
    fn workflow_artifacts_are_not_source() {
        for rel in [
            "specs/my-feature/state.json",
            "specs/my-feature/lease.json",
            "specs/my-feature/spec.md",
            "specs/my-feature/breakdown.md",
        ] {
            assert!(is_workflow_artifact(rel), "{rel} must be an artifact");
        }
    }

    /// A hand-written document under `specs/` IS source — excluding the whole tree
    /// would hide real design notes.
    #[test]
    fn hand_written_spec_documents_are_source() {
        assert!(!is_workflow_artifact("specs/my-feature/notes.md"));
        assert!(!is_workflow_artifact("specs/my-feature/diagram.svg"));
    }

    /// `specs/` is the only prefix that carries artifacts; a `state.json` elsewhere is
    /// ordinary project source.
    #[test]
    fn artifact_names_outside_specs_are_source() {
        assert!(!is_workflow_artifact("src/state.json"));
        assert!(!is_workflow_artifact("config/lease.json"));
    }

    #[test]
    fn test_files_are_recognized() {
        for rel in [
            "tests/foo.rs",
            "src/tests/foo.rs",
            "src/test_thing.py",
            "src/thing.test.js",
            "src/thing_test.go",
            "src/thing.spec.ts",
        ] {
            assert!(is_test_file(rel), "{rel} must read as a test");
        }
    }

    #[test]
    fn ordinary_sources_are_not_test_files() {
        for rel in ["src/main.rs", "src/lib.rs", "app/latest.py"] {
            assert!(!is_test_file(rel), "{rel} must not read as a test");
        }
    }
}
