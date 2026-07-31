//! Path anchors: `<!--@ crates/sc-web/src/mirror_server.rs -->`.
//!
//! The simplest and most robust anchor, and deliberately **language-agnostic** —
//! a spec governing a config file, an HTML template or a TOML pack is checkable
//! exactly like one governing Rust (spec 17). There is no parsing here, only a
//! filesystem question, so there is no case where the checker is limited and
//! therefore no `Unknown` outcome.

use std::path::Path;

use super::{Located, Resolution};

/// Does `rel` exist under `root`?
///
/// Directories resolve, not just files: a spec governing `crates/sc-comply/packs`
/// is making a real claim about a real thing, and rejecting it would push authors
/// toward naming an arbitrary file inside it instead.
pub fn resolve(rel: &str, root: &Path) -> Resolution {
    // Reject traversal rather than following it. An anchor is a claim about this
    // repository; `../` leaves it, and answering for something outside the
    // workspace would be a claim the checker has no business making.
    if rel.split('/').any(|seg| seg == "..") {
        return Resolution::broken(format!(
            "{rel:?} escapes the workspace — an anchor names a path inside the repo"
        ));
    }

    if root.join(rel).exists() {
        Resolution::ok(vec![Located::file(rel)])
    } else {
        Resolution::broken(format!("no such path: {rel}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::ClaimStatus;
    use crate::test_support::{repo_root, temp_repo, write};

    #[test]
    fn an_existing_path_resolves() {
        let root = temp_repo("path-ok");
        write(&root, "crates/a/src/lib.rs", "fn a() {}\n");
        let r = resolve("crates/a/src/lib.rs", &root);
        assert_eq!(r.status, ClaimStatus::Ok);
        assert_eq!(r.targets, vec![Located::file("crates/a/src/lib.rs")]);
        assert!(r.note.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_path_is_broken_and_says_which() {
        let root = temp_repo("path-gone");
        let r = resolve("crates/a/src/gone.rs", &root);
        assert_eq!(r.status, ClaimStatus::Broken);
        assert!(r.note.unwrap().contains("gone.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_resolves_like_a_file() {
        let root = temp_repo("path-dir");
        write(&root, "packs/soc2/pack.toml", "x = 1\n");
        assert_eq!(resolve("packs/soc2", &root).status, ClaimStatus::Ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_source_files_resolve_because_paths_are_language_agnostic() {
        // Spec 17: "path anchors work regardless of language". Real anchors point
        // at `dashboard.html`, and `sc-index` would never index it.
        let root = repo_root();
        for rel in [
            "crates/sc-web/src/dashboard.html",
            "Cargo.toml",
            "scripts/check.sh",
        ] {
            assert_eq!(resolve(rel, &root).status, ClaimStatus::Ok, "{rel}");
        }
    }

    #[test]
    fn a_traversing_path_is_rejected_rather_than_followed() {
        // An anchor is a claim about THIS repo. Answering for something outside
        // it would be a claim the checker has no standing to make.
        let root = temp_repo("path-escape");
        let r = resolve("../../etc/passwd", &root);
        assert_eq!(r.status, ClaimStatus::Broken);
        assert!(r.note.unwrap().contains("escapes"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_real_path_anchor_in_this_repo_resolves() {
        // The eight path anchors live in specs 18-20 today. If one rots, this
        // fails — which is the tool checking itself.
        let root = repo_root();
        for rel in [
            "crates/sc-web/src/mirror_server.rs",
            "crates/sc-win/src/session/mod.rs",
            "crates/sc-workflow/src/state.rs",
            "crates/sc-workflow/src/gate.rs",
        ] {
            assert_eq!(resolve(rel, &root).status, ClaimStatus::Ok, "{rel}");
        }
    }
}
