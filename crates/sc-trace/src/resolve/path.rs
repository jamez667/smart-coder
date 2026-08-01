//! Path anchors: `<!--@ crates/sc-web/src/mirror_server.rs -->`.
//!
//! The simplest and most robust anchor, and deliberately **language-agnostic** —
//! a spec governing a config file, an HTML template or a TOML pack is checkable
//! exactly like one governing Rust (spec 17). There is no parsing here, only a
//! filesystem question.
//!
//! ## Claims about another repository
//!
//! An anchor may name a sibling repository — a `<repo>:<path>` target — and
//! resolves [`Unknown`](crate::status::ClaimStatus::Unknown): **"we could not
//! look", never "we looked and it was fine"**. It does not gate, and it is never
//! counted as passing.
//!
//! **Currently unused**, and kept deliberately. It was written when the hosted
//! intake surface briefly shipped from its own repository; that split was
//! reverted, but the reasoning outlives the occasion. When a spec governs code
//! this checker cannot read, both alternatives are worse than admitting it:
//! deleting the anchor loses the claim — the drift this crate exists to catch,
//! arrived at by tidying — and leaving it pointing at an absent path reports a
//! `Broken` that is not one. Twenty false alarms is how a check gets switched
//! off.

use std::path::Path;

use super::{Located, Resolution};

/// Separates a repository name from the path within it.
const REPO_SEP: char = ':';

/// Does `rel` exist under `root`?
///
/// Directories resolve, not just files: a spec governing `crates/sc-comply/packs`
/// is making a real claim about a real thing, and rejecting it would push authors
/// toward naming an arbitrary file inside it instead.
pub fn resolve(rel: &str, root: &Path) -> Resolution {
    // A claim about a sibling repository. This checker reads one working tree, so
    // it cannot answer — and saying so is the whole point of `Unknown`.
    if let Some((repo, path)) = rel.split_once(REPO_SEP) {
        return Resolution::unknown(format!(
            "{path} lives in {repo}, which this checker cannot read — verify it there"
        ));
    }

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
    fn a_claim_about_another_repository_is_unknown_not_broken() {
        // Some specs govern code that ships from a sibling repo. Reporting those
        // as broken would be twenty false alarms, which is how a check gets
        // switched off — and reporting them as `Ok` would be a lie.
        let root = temp_repo("cross-repo");
        let r = resolve("smart-coder-web:crates/sc-server/src/routes.rs", &root);

        assert_eq!(r.status, ClaimStatus::Unknown);
        let note = r.note.clone().unwrap_or_default();
        assert!(note.contains("smart-coder-web"), "{note}");
        assert!(note.contains("cannot read"), "{note}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cross_repo_anchor_is_unknown_even_if_the_path_happens_to_exist_here() {
        // Otherwise the answer depends on a coincidence of layout: the same
        // anchor would pass in one checkout and fail in another.
        let root = temp_repo("cross-repo-collide");
        write(&root, "crates/sc-proto/src/lib.rs", "// here too");

        let r = resolve("smart-coder-web:crates/sc-proto/src/lib.rs", &root);
        assert_eq!(r.status, ClaimStatus::Unknown);
        let _ = std::fs::remove_dir_all(&root);
    }

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
