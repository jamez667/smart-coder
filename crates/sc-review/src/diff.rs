//! The integrated diff, in hunks (spec 16 — "the reviewed artifact must be the
//! one that ships").
//!
//! The swarm never builds a unified diff today: `integrate` snapshots each file
//! before writing and holds the merged text after, so the review is handed
//! `(path, before, after)` triples and derives the hunks here. That keeps the
//! diff pure data — buildable in a test from two string literals, with no git,
//! no external process and no filesystem.
//!
//! Hunks matter because they are how a finding is anchored: a reviewer is shown a
//! numbered list and *selects* from it, which is a far easier task than counting
//! lines, and one it degrades gracefully at (spec 16 — "Anchoring").

use serde::{Deserialize, Serialize};

/// Identifies one hunk within one file's diff. Stable for the life of a review:
/// it is an index into the hunk list the reviewer was shown, so a model can only
/// ever pick one it saw, or pick nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HunkId(pub usize);

impl std::fmt::Display for HunkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "H{}", self.0)
    }
}

/// One contiguous run of changed lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub id: HunkId,
    /// 1-based line in the *new* file where this hunk's changed region starts.
    /// A pure deletion points at the line it was removed from.
    pub new_start: usize,
    /// Lines removed (from the old file), in order.
    pub removed: Vec<String>,
    /// Lines added (to the new file), in order.
    pub added: Vec<String>,
}

impl Hunk {
    /// The added lines as one block — what a lens actually reads.
    pub fn added_text(&self) -> String {
        self.added.join("\n")
    }
}

/// One file's changes within the integrated diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// Workspace-relative path, `/`-separated.
    pub path: String,
    /// The file's full contents before the change. `None` when the file is new.
    ///
    /// Carried alongside `after` because the useful question — *which symbols did
    /// this diff add?* — is answered by comparing the two parsed files, not by
    /// parsing the added lines. A function inserted mid-file has added lines that
    /// begin with a dangling `}` and parse as nothing at all; the whole file
    /// always parses. See [`crate::ground::added_symbol_names`].
    pub before: Option<String>,
    /// The file's full contents after the change. `None` when the file was
    /// deleted. Carried because "abstraction fit" needs the surrounding file,
    /// not just the hunk (spec 16 — grounding).
    pub after: Option<String>,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Total changed lines in this file — the size measure the skip threshold uses.
    pub fn changed_lines(&self) -> usize {
        self.hunks
            .iter()
            .map(|h| h.removed.len() + h.added.len())
            .sum()
    }
}

/// The whole integrated diff for one subtask.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratedDiff {
    pub files: Vec<FileDiff>,
}

impl IntegratedDiff {
    /// Build a diff from `(path, before, after)` triples. `before == None` is a
    /// new file, `after == None` a deletion. Files whose contents did not change
    /// are dropped — an unchanged file is not part of a diff.
    pub fn from_changes<'a>(
        changes: impl IntoIterator<Item = (&'a str, Option<&'a str>, Option<&'a str>)>,
    ) -> Self {
        let mut files = Vec::new();
        for (path, before, after) in changes {
            if before == after {
                continue;
            }
            let hunks = hunks_between(before.unwrap_or(""), after.unwrap_or(""));
            if hunks.is_empty() {
                continue;
            }
            files.push(FileDiff {
                path: path.replace('\\', "/"),
                before: before.map(str::to_string),
                after: after.map(str::to_string),
                hunks,
            });
        }
        Self { files }
    }

    /// Total changed lines across every file.
    pub fn changed_lines(&self) -> usize {
        self.files.iter().map(FileDiff::changed_lines).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Look up a file's diff by path.
    pub fn file(&self, path: &str) -> Option<&FileDiff> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Render the diff the way a reviewer is shown it: each hunk labelled with the
    /// id it must cite. Nothing here invites the model to count lines.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for f in &self.files {
            out.push_str(&format!("=== {} ===\n", f.path));
            for h in &f.hunks {
                out.push_str(&format!(
                    "--- hunk {} (at line {}) ---\n",
                    h.id, h.new_start
                ));
                for l in &h.removed {
                    out.push_str(&format!("- {l}\n"));
                }
                for l in &h.added {
                    out.push_str(&format!("+ {l}\n"));
                }
            }
        }
        out
    }
}

/// Line-level diff of two file bodies into contiguous changed runs.
///
/// A common-prefix/common-suffix scan, not a full LCS. That is the right trade
/// here: the input is one file before and after a merge, where changes cluster,
/// and the cost of being coarse is a slightly larger hunk — which a reviewer
/// reads perfectly well. An LCS would split it more finely and buy nothing,
/// because nothing downstream matches on hunk *content*.
fn hunks_between(before: &str, after: &str) -> Vec<Hunk> {
    let old: Vec<&str> = split_lines(before);
    let new: Vec<&str> = split_lines(after);

    let mut prefix = 0usize;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let removed: Vec<String> = old[prefix..old.len() - suffix]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let added: Vec<String> = new[prefix..new.len() - suffix]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if removed.is_empty() && added.is_empty() {
        return Vec::new();
    }
    vec![Hunk {
        id: HunkId(0),
        new_start: prefix + 1,
        removed,
        added,
    }]
}

/// Split into lines with CRLF normalized away and no phantom trailing empty line.
fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    s.trim_end_matches('\n')
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_added_block_becomes_one_hunk_anchored_in_the_new_file() {
        let before = "fn a() {}\nfn c() {}\n";
        let after = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let d = IntegratedDiff::from_changes([("src/lib.rs", Some(before), Some(after))]);

        let f = d.file("src/lib.rs").expect("file present");
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].added, vec!["fn b() {}"]);
        assert!(f.hunks[0].removed.is_empty());
        // Line 2 of the NEW file — the first changed line, not a guess.
        assert_eq!(f.hunks[0].new_start, 2);
        assert_eq!(d.changed_lines(), 1);
    }

    #[test]
    fn a_new_file_is_all_addition_and_a_deletion_is_all_removal() {
        let d = IntegratedDiff::from_changes([
            ("new.rs", None, Some("fn n() {}\n")),
            ("gone.rs", Some("fn g() {}\n"), None),
        ]);
        let n = d.file("new.rs").unwrap();
        assert_eq!(n.hunks[0].added, vec!["fn n() {}"]);
        assert!(n.hunks[0].removed.is_empty());
        // A new file has no `before` — nothing it could have defined already.
        assert!(n.before.is_none());
        let g = d.file("gone.rs").unwrap();
        assert!(g.hunks[0].added.is_empty());
        assert_eq!(g.hunks[0].removed, vec!["fn g() {}"]);
        // A deleted file carries no `after` for the abstraction-fit lens to read.
        assert!(g.after.is_none());
        assert_eq!(g.before.as_deref(), Some("fn g() {}\n"));
    }

    #[test]
    fn an_unchanged_file_is_not_part_of_the_diff() {
        // Integration re-writes files it merged even when the merge was a no-op;
        // reviewing an unchanged file would waste a model call on nothing.
        let d = IntegratedDiff::from_changes([
            ("same.rs", Some("fn a() {}\n"), Some("fn a() {}\n")),
            // Trailing-newline-only difference is likewise not a change worth reviewing.
            ("nl.rs", Some("fn a() {}"), Some("fn a() {}\n")),
        ]);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(d.changed_lines(), 0);
    }

    #[test]
    fn crlf_does_not_manufacture_a_diff() {
        // The merge normalizes line endings; a Windows checkout must not read as
        // "every line changed".
        let d = IntegratedDiff::from_changes([("w.rs", Some("a\r\nb\r\n"), Some("a\nb\n"))]);
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn render_labels_each_hunk_with_the_id_a_reviewer_must_cite() {
        let d = IntegratedDiff::from_changes([("src/x.rs", Some("old\n"), Some("new\n"))]);
        let text = d.render();
        assert!(text.contains("=== src/x.rs ==="), "{text}");
        assert!(text.contains("--- hunk H0 (at line 1) ---"), "{text}");
        assert!(text.contains("- old"), "{text}");
        assert!(text.contains("+ new"), "{text}");
    }

    #[test]
    fn backslash_paths_are_normalized() {
        let d = IntegratedDiff::from_changes([("src\\win.rs", Some("a\n"), Some("b\n"))]);
        assert!(d.file("src/win.rs").is_some(), "{d:?}");
    }
}
