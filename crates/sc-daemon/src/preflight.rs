//! Is this repository safe to write into right now?
//!
//! Nothing on the workflow path inspects git today — the CLI checks backend
//! liveness, the GUI warns about the sandbox, neither looks at the tree. That is
//! survivable for someone sitting in front of the repository. It is not survivable
//! for a task claimed at 3am against a tree left mid-rebase (spec 19).
//!
//! The drafted spec lands in `specs/<slug>/`, **inside the repository**, so an
//! unattended run adds tracked files to whatever state the tree is in.
//!
//! Two rules, and the second matters as much as the first:
//!
//! * **Refuse an interrupted operation** — a rebase, merge, cherry-pick or bisect
//!   in progress. Writing into one produces a mess a human then has to
//!   disentangle, in a repository they were already halfway through fixing.
//! * **A merely dirty tree is fine and must not block.** Phases write only under
//!   `specs/<slug>/`, and refusing on uncommitted work would make the daemon
//!   useless on any real working repository — which is every repository anyone
//!   actually files a task against.
//!
//! Detection reads marker files under `.git/` rather than shelling out. That is
//! how git itself records these states, it needs no dependency, and it cannot
//! block on a git process that decides to prompt.

use std::path::Path;

/// Why a repository is not safe to write into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    /// The path is not a directory, or holds no `.git`.
    NotARepo,
    /// A git operation is in progress.
    Interrupted {
        /// `rebase`, `merge`, `cherry-pick`, `revert`, `bisect`.
        operation: &'static str,
    },
}

impl NotReady {
    /// What to tell the developer. Says what is wrong *and* what to do, because a
    /// task refused overnight is read hours later with no context.
    pub fn reason(&self, repo: &Path) -> String {
        match self {
            NotReady::NotARepo => format!(
                "{} is not a git repository — the drafted spec would land in an \
                 untracked directory with no way to review it as a diff.",
                repo.display()
            ),
            NotReady::Interrupted { operation } => format!(
                "{} has a {operation} in progress. Finish or abort it, then requeue \
                 — writing a spec into a half-applied operation makes a mess you \
                 would have to disentangle by hand.",
                repo.display()
            ),
        }
    }
}

/// The `.git/` entries git leaves while an operation is in flight, and the name
/// to report for each.
///
/// `rebase-merge` and `rebase-apply` are directories; the rest are files. Both are
/// covered by an `exists()` check.
const MARKERS: &[(&str, &str)] = &[
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    ("BISECT_LOG", "bisect"),
];

/// Can the daemon write a spec into `repo` right now?
pub fn check(repo: &Path) -> std::result::Result<(), NotReady> {
    let git = repo.join(".git");
    // A worktree or submodule records `.git` as a *file* pointing elsewhere. Both
    // are real repositories, so presence is the test rather than being a directory.
    if !repo.is_dir() || !git.exists() {
        return Err(NotReady::NotARepo);
    }
    for (marker, operation) in MARKERS {
        if git.join(marker).exists() {
            return Err(NotReady::Interrupted { operation });
        }
    }
    // Deliberately no dirty-tree check. See the module docs: refusing on
    // uncommitted work would make the daemon useless on any real repository.
    Ok(())
}

/// Is `repo` ready?
pub fn is_ready(repo: &Path) -> bool {
    check(repo).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{interrupt, temp_dir, temp_repo};

    #[test]
    fn a_clean_repository_is_ready() {
        let repo = temp_repo("pf-clean");
        assert!(check(&repo).is_ok());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_merely_dirty_tree_still_runs() {
        // THE rule that keeps the daemon usable. Every real repository someone
        // files a task against has uncommitted work in it; refusing would make
        // the feature useless in exactly the case it exists for.
        let repo = temp_repo("pf-dirty");
        std::fs::write(repo.join("src.rs"), "half-finished work\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "scratch\n").unwrap();
        assert!(check(&repo).is_ok(), "uncommitted work must not block");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn every_interrupted_operation_is_refused_by_name() {
        // Named individually because "something is in progress" leaves a
        // developer reading the note hours later with nothing to act on.
        for (marker, expected) in [
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase"),
            ("MERGE_HEAD", "merge"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
            ("BISECT_LOG", "bisect"),
        ] {
            let repo = temp_repo("pf-interrupted");
            interrupt(&repo, marker);
            assert_eq!(
                check(&repo),
                Err(NotReady::Interrupted {
                    operation: expected
                }),
                "{marker} should report as {expected}"
            );
            let _ = std::fs::remove_dir_all(&repo);
        }
    }

    #[test]
    fn the_refusal_says_what_is_wrong_and_what_to_do() {
        let repo = temp_repo("pf-reason");
        interrupt(&repo, "MERGE_HEAD");
        let reason = check(&repo).unwrap_err().reason(&repo);
        assert!(reason.contains("merge in progress"), "{reason}");
        assert!(
            reason.contains("requeue"),
            "an overnight refusal must say how to proceed: {reason}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_refused() {
        // The spec would land in an untracked directory, unreviewable as a diff.
        let dir = temp_dir("pf-notrepo");
        assert_eq!(check(&dir), Err(NotReady::NotARepo));
        assert_eq!(check(&dir.join("nope")), Err(NotReady::NotARepo));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_whose_git_is_a_file_is_still_a_repository() {
        // Linked worktrees and submodules record `.git` as a file pointing
        // elsewhere. Requiring a directory would refuse a perfectly good repo.
        let dir = temp_dir("pf-worktree");
        std::fs::write(dir.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();
        assert!(check(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finishing_the_operation_makes_the_repository_ready_again() {
        let repo = temp_repo("pf-recovers");
        interrupt(&repo, "MERGE_HEAD");
        assert!(!is_ready(&repo));
        std::fs::remove_file(repo.join(".git").join("MERGE_HEAD")).unwrap();
        assert!(is_ready(&repo), "a requeued task can now run");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
