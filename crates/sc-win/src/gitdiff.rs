//! Live git-diff line tracking for the code view: which lines of a file differ from its
//! committed (HEAD) state right now. Used to highlight changed lines GitHub-PR-style AS the
//! agent edits — ground truth from git, not inferred from events.
//!
//! The hunk-header parsing is pure/host-testable; the app runs `git diff -U0` each tick and
//! feeds the output here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A GitHub-PR-style diff of ONE file vs HEAD, positioned onto the CURRENT file's line numbers:
/// which current lines are added (green), and — for each current line — the removed (red) lines
/// that were deleted just BEFORE it (rendered as red rows above that line).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Current-file line numbers that are added/changed (green). 1-based.
    pub added: BTreeSet<usize>,
    /// Removed (red) lines, keyed by the current-file line number they sit BEFORE. A hunk that
    /// deletes at end-of-file keys on `last_line + 1`. Values are the removed line texts, in order.
    pub removed_before: BTreeMap<usize, Vec<String>>,
}

/// One contiguous changed region (a "block", VS-Code-style), positioned on the current file: the
/// current line range it occupies (`cur_start..=cur_end`, 1-based; empty when the hunk is a pure
/// deletion) and the exact HEAD text that reverting the block restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// First current-file line of the block (1-based). For a pure deletion this is the line the
    /// removed text sat before (the splice-insert point).
    pub cur_start: usize,
    /// Last current-file line of the block (1-based, inclusive). `cur_start - 1` for a pure
    /// deletion (an empty current range → the revert is an insertion).
    pub cur_end: usize,
    /// The HEAD (committed) text of this block — what a "revert block" splices back in. Empty
    /// string for a pure ADDITION (revert = delete the added lines).
    pub head_text: String,
}

impl FileDiff {
    /// Group the diff into [`DiffHunk`] blocks (like VS Code's gutter change regions), each
    /// carrying the HEAD text to revert just that block. Added lines and removed lines that touch
    /// — i.e. are separated by NO unchanged line — are merged into ONE block, so a comment whose
    /// old N lines became M lines is a single revertable region (not one button per green run).
    pub fn hunks(&self) -> Vec<DiffHunk> {
        // The set of current lines that are part of ANY change: added lines, plus each removed
        // block's anchor (the line the deletion sits before). Two changes with no unchanged line
        // between them are one block.
        let mut points: BTreeSet<usize> = self.added.iter().copied().collect();
        for &anchor in self.removed_before.keys() {
            points.insert(anchor);
        }
        if points.is_empty() {
            return Vec::new();
        }
        // Walk the change-points, merging any that are adjacent (gap of 1 line = touching, since a
        // removed block's anchor equals the next kept line). A gap > 1 means an unchanged line sits
        // between → separate block.
        let mut blocks: Vec<(usize, usize)> = Vec::new(); // (first_point, last_point)
        for p in points {
            match blocks.last_mut() {
                Some((_, last)) if p <= *last + 1 => *last = p,
                _ => blocks.push((p, p)),
            }
        }
        blocks
            .into_iter()
            .map(|(lo, hi)| {
                // Current (green) range = the added lines within [lo, hi]. If none, it's a pure
                // deletion → empty current range (revert re-inserts before `lo`).
                let added_lo = (lo..=hi).find(|n| self.added.contains(n));
                let added_hi = (lo..=hi).rev().find(|n| self.added.contains(n));
                let (cur_start, cur_end) = match (added_lo, added_hi) {
                    (Some(a), Some(b)) => (a, b),
                    _ => (lo, lo.saturating_sub(1)), // empty range
                };
                // HEAD text = every removed block anchored within this region, in order.
                let head: Vec<String> = (lo..=hi)
                    .filter_map(|n| self.removed_before.get(&n))
                    .flatten()
                    .cloned()
                    .collect();
                DiffHunk {
                    cur_start,
                    cur_end,
                    head_text: head.join("\n"),
                }
            })
            .collect()
    }
}

/// Parse `git diff -U0` output into a [`FileDiff`]: added current-line numbers (green) and, per
/// hunk, the removed lines (red) anchored to the current line they were deleted before. Handles
/// the hunk header `@@ -a,b +c,d @@` plus the `-`/`+` content lines that follow it.
pub fn parse_file_diff(diff: &str) -> FileDiff {
    let mut out = FileDiff::default();
    let mut cur_new = 0usize; // next current-file line number within the active hunk
    let mut pending_removed: Vec<String> = Vec::new();
    let mut anchor = 0usize; // current line the pending removals sit before
    let flush = |anchor: usize, removed: &mut Vec<String>, out: &mut FileDiff| {
        if !removed.is_empty() {
            out.removed_before
                .entry(anchor.max(1))
                .or_default()
                .append(removed);
        }
    };
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            // New hunk: flush any pending removals from the previous one.
            flush(anchor, &mut pending_removed, &mut out);
            // Parse the `+c[,d]` new-side start so we can number added lines.
            let plus = rest.split_whitespace().find(|t| t.starts_with('+'));
            cur_new = plus
                .map(|p| p[1..].split(',').next().unwrap_or("0").parse().unwrap_or(0))
                .unwrap_or(0);
            anchor = cur_new; // removals in this hunk sit before its first new line
            continue;
        }
        // File headers aren't content.
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                if cur_new > 0 {
                    out.added.insert(cur_new);
                    cur_new += 1;
                }
            }
            Some(b'-') => {
                pending_removed.push(line[1..].to_string());
            }
            _ => {
                // A context line (shouldn't appear with -U0, but be safe): advances new-line count.
                if cur_new > 0 {
                    cur_new += 1;
                }
            }
        }
    }
    flush(anchor, &mut pending_removed, &mut out);
    out
}

/// The PR-style [`FileDiff`] for one file in `workspace` vs HEAD. For an UNTRACKED file (git won't
/// diff it) the whole file reads as added. Empty diff if the file is clean / git is absent.
pub fn file_diff(workspace: &Path, rel: &str) -> FileDiff {
    // Diff against HEAD (not the index) so BOTH staged and unstaged changes show — the PR view is
    // "everything different from the last commit", matching the "changed vs HEAD" header.
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["diff", "-U0", "--no-color", "HEAD", "--", rel])
        .output();
    if let Ok(o) = &out {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            if !text.trim().is_empty() {
                return parse_file_diff(&text);
            }
        }
    }
    // No tracked diff: if the file is untracked (present in `git status` as ??), treat every line
    // as added (green), like GitHub shows a brand-new file.
    if is_untracked(workspace, rel) {
        if let Ok(text) = std::fs::read_to_string(workspace.join(rel)) {
            let n = text.lines().count().max(1);
            return FileDiff {
                added: (1..=n).collect(),
                removed_before: BTreeMap::new(),
            };
        }
    }
    FileDiff::default()
}

/// Whether `rel` is untracked in `workspace` (a `??` entry in `git status --porcelain`).
fn is_untracked(workspace: &Path, rel: &str) -> bool {
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain", "--", rel])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.starts_with("??")),
        _ => false,
    }
}

/// Parse `git diff -U0` output into the set of **current-file** line numbers that are
/// added/changed. Uses the hunk headers `@@ -a,b +c,d @@`: the `+c,d` part names the new
/// lines (c..c+d-1). A `+c` with no count means one line at c. Deletions (`d == 0`) add no
/// current lines. 1-based, matching the code view's line numbering.
pub fn changed_lines(diff: &str) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        // rest looks like "-a,b +c,d @@ optional context"
        let Some(plus) = rest.split_whitespace().find(|t| t.starts_with('+')) else {
            continue;
        };
        let plus = &plus[1..]; // drop '+'
        let (start, count) = match plus.split_once(',') {
            Some((s, c)) => (
                s.parse::<usize>().unwrap_or(0),
                c.parse::<usize>().unwrap_or(0),
            ),
            None => (plus.parse::<usize>().unwrap_or(0), 1),
        };
        if start == 0 || count == 0 {
            continue; // pure deletion (no current-file lines) or unparseable
        }
        for n in start..start + count {
            set.insert(n);
        }
    }
    set
}

/// Whether a `git diff` touches ONLY comments/whitespace — i.e. every added or removed
/// content line (`+`/`-`, excluding the `+++`/`---` file headers) is a Rust comment (`//`,
/// `///`, `//!`, or inside a `/* */` block) or blank. Used to skip `cargo check` on a
/// comment-only edit (the compiler ignores comments, so it can't break the build).
///
/// Conservative: returns false if there are NO content changes (nothing to skip for), or if
/// any changed line looks like real code. `/* */` handling is line-local (a `+`/`-` line that
/// opens or lives inside a block comment); a diff that only shows a fragment of a multi-line
/// block comment still reads as comment lines here, which is the safe direction.
pub fn is_comment_only_change(diff: &str) -> bool {
    let mut saw_content = false;
    for line in diff.lines() {
        // Skip the file headers (+++ b/x, --- a/x) and hunk headers.
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        let content = match line.as_bytes().first() {
            Some(b'+') | Some(b'-') => &line[1..],
            _ => continue, // context / metadata line
        };
        saw_content = true;
        if !is_comment_or_blank(content) {
            return false;
        }
    }
    saw_content
}

/// Whether a single line of Rust is a comment or blank (whitespace only). Recognizes line
/// comments (`//`, `///`, `//!`) and a line that is entirely a `/* … */` or opens/continues a
/// block comment (`/*`, `*`, `*/`). Anything with code before a trailing `//` is NOT
/// comment-only (that's a code change with a comment).
fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
    // block-comment continuation / close (`*`, `*/`)
}

/// Run `git diff -U0` for a single file in `workspace` and return the changed current-file
/// line numbers. Empty if the tree is clean for that file, git isn't present, or the file
/// isn't tracked. `rel` is the workspace-relative path.
pub fn file_changed_lines(workspace: &Path, rel: &str) -> BTreeSet<usize> {
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["diff", "-U0", "--no-color", "--", rel])
        .output();
    match out {
        Ok(o) if o.status.success() => changed_lines(&String::from_utf8_lossy(&o.stdout)),
        _ => BTreeSet::new(),
    }
}

/// A file's working-tree status vs HEAD, for the PR-style file tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// New / untracked (GitHub green "A").
    Added,
    /// Modified (amber "M").
    Modified,
    /// Deleted (red "D").
    Deleted,
}

impl FileStatus {
    /// The single-letter badge (A/M/D).
    pub fn badge(self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
        }
    }
}

/// Parse `git status --porcelain` into a map of workspace-relative path → [`FileStatus`].
/// The XY code: `??` = untracked (Added), any `D` = Deleted, else Modified. Handles rename
/// (`R  old -> new`) by marking the new path Modified.
pub fn parse_status(porcelain: &str) -> std::collections::BTreeMap<String, FileStatus> {
    let mut map = std::collections::BTreeMap::new();
    for line in porcelain.lines() {
        if line.len() < 3 {
            continue;
        }
        let code = &line[..2];
        let path = line[3..].trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        let path = path.trim_matches('"').replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        let status = if code == "??" || code.contains('A') {
            FileStatus::Added
        } else if code.contains('D') {
            FileStatus::Deleted
        } else {
            FileStatus::Modified
        };
        map.insert(path, status);
    }
    map
}

/// Whether a file has staged (index) changes, unstaged (working-tree) changes, or both — read
/// from the two-column `git status --porcelain` XY code. Used to decide which of Stage / Unstage
/// to offer in the git-tab context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageState {
    /// The index column (X) shows a change: something is staged.
    pub staged: bool,
    /// The worktree column (Y) shows a change (or the file is untracked): something is unstaged.
    pub unstaged: bool,
}

/// Parse `git status --porcelain` into path → [`StageState`]. The XY code: X (col 0) is the
/// index/staged state, Y (col 1) the worktree/unstaged state; a space means "no change" in that
/// column. `??` (untracked) counts as fully unstaged. Rename lines map to the new path.
pub fn parse_stage_states(porcelain: &str) -> std::collections::BTreeMap<String, StageState> {
    let mut map = std::collections::BTreeMap::new();
    for line in porcelain.lines() {
        if line.len() < 3 {
            continue;
        }
        let bytes = line.as_bytes();
        let (x, y) = (bytes[0] as char, bytes[1] as char);
        let path = line[3..].trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        let path = path.trim_matches('"').replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        let untracked = x == '?' && y == '?';
        let state = StageState {
            staged: !untracked && x != ' ',
            unstaged: untracked || y != ' ',
        };
        map.insert(path, state);
    }
    map
}

/// The staged/unstaged state of every changed file in `workspace` (path → [`StageState`]). Empty
/// if the tree is clean or git isn't present.
pub fn stage_states(workspace: &Path) -> std::collections::BTreeMap<String, StageState> {
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            parse_stage_states(&String::from_utf8_lossy(&o.stdout))
        }
        _ => std::collections::BTreeMap::new(),
    }
}

/// The working-tree status of every changed file in `workspace` (path → status). Empty if the
/// tree is clean or git isn't present.
pub fn statuses(workspace: &Path) -> std::collections::BTreeMap<String, FileStatus> {
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) if o.status.success() => parse_status(&String::from_utf8_lossy(&o.stdout)),
        _ => std::collections::BTreeMap::new(),
    }
}

/// Added/removed line counts for one file, from `git diff --numstat`. Git counts a modified
/// line as one deletion + one insertion, so there's no separate "changed" figure — `+ins −del`
/// is the honest shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineDelta {
    /// Inserted lines (the `+` count).
    pub added: usize,
    /// Deleted lines (the `−` count).
    pub removed: usize,
}

/// Parse `git diff --numstat` output (`<added>\t<removed>\t<path>` per line) into path → counts.
/// Binary files report `-\t-\t<path>`; those are skipped (no meaningful line delta). Rename lines
/// (`a\tb\told => new` / brace form) map to the trailing path.
pub fn parse_numstat(numstat: &str) -> std::collections::BTreeMap<String, LineDelta> {
    let mut map = std::collections::BTreeMap::new();
    for line in numstat.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // Binary files show "-" for both counts; skip them.
        let (Ok(added), Ok(removed)) = (a.parse::<usize>(), d.parse::<usize>()) else {
            continue;
        };
        // A rename shows as "old => new" (possibly with `{}` braces); take the resulting path.
        let path = path.rsplit(" => ").next().unwrap_or(path);
        let path = path.trim_end_matches('}').trim_matches('"').replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        map.insert(path, LineDelta { added, removed });
    }
    map
}

/// Per-file added/removed line counts for `workspace`. When `cached` is true, counts the STAGED
/// diff (`--cached`); otherwise the unstaged working-tree diff. Empty if git isn't present.
pub fn line_deltas(
    workspace: &Path,
    cached: bool,
) -> std::collections::BTreeMap<String, LineDelta> {
    let mut args = vec!["diff", "--numstat", "--no-color"];
    if cached {
        args.push("--cached");
    }
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(&args)
        .output();
    match out {
        Ok(o) if o.status.success() => parse_numstat(&String::from_utf8_lossy(&o.stdout)),
        _ => std::collections::BTreeMap::new(),
    }
}

/// The current branch name (e.g. `main`), or `None` if detached / not a git repo.
pub fn current_branch(workspace: &Path) -> Option<String> {
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name)
    }
}

/// Where the local branch sits relative to its upstream tracking branch: how many commits it's
/// ahead (to push) and behind (to pull). `None` for `upstream` when the branch has no tracking
/// branch (nothing to push/pull against).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamStatus {
    /// The upstream ref name (e.g. `origin/main`), if the branch tracks one.
    pub upstream: Option<String>,
    /// Commits on HEAD not on upstream — i.e. unpushed (the ↑ count).
    pub ahead: usize,
    /// Commits on upstream not on HEAD — i.e. unpulled (the ↓ count).
    pub behind: usize,
}

/// Read the branch's [`UpstreamStatus`] (ahead/behind vs its tracking branch). Uses cached
/// remote-tracking refs — it does NOT hit the network (run a fetch first for fresh behind counts).
/// Returns a default (no upstream, 0/0) when there's no tracking branch or git is absent.
pub fn upstream_status(workspace: &Path) -> UpstreamStatus {
    let git = |args: &[&str]| {
        crate::proc::git()
            .arg("-C")
            .arg(workspace)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // The upstream ref name; absent → no tracking branch.
    let Some(upstream) = git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]) else {
        return UpstreamStatus::default();
    };
    // `--count` prints "behind<TAB>ahead" for `@{u}...HEAD` (left=upstream-only, right=HEAD-only).
    let (ahead, behind) = git(&["rev-list", "--left-right", "--count", "@{u}...HEAD"])
        .and_then(|s| {
            let mut it = s.split_whitespace();
            let behind = it.next()?.parse().ok()?;
            let ahead = it.next()?.parse().ok()?;
            Some((ahead, behind))
        })
        .unwrap_or((0, 0));
    UpstreamStatus {
        upstream: Some(upstream),
        ahead,
        behind,
    }
}

/// The raw `git diff` for `files` in `workspace` (with content lines), for content analysis
/// like [`is_comment_only_change`]. Empty string if git isn't present / nothing changed.
pub fn files_diff(workspace: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let out = crate::proc::git()
        .arg("-C")
        .arg(workspace)
        .args(["diff", "--no-color", "--"])
        .args(files)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_added_and_changed_line_ranges() {
        // One line added at 41; a 3-line change starting at 100.
        let diff = "\
diff --git a/x b/x
@@ -40,0 +41 @@ fn main() {
+// new line
@@ -100,3 +100,3 @@ fn f() {
-old a
-old b
-old c
+new a
+new b
+new c
";
        let ch = changed_lines(diff);
        assert!(ch.contains(&41), "single added line: {ch:?}");
        assert!(
            ch.contains(&100) && ch.contains(&101) && ch.contains(&102),
            "3-line change: {ch:?}"
        );
        assert!(!ch.contains(&40) && !ch.contains(&103), "bounded: {ch:?}");
    }

    #[test]
    fn file_diff_parses_added_and_removed_with_positions() {
        // Line 41 added; lines 100-102 replaced (3 removed, 3 added).
        let diff = "\
diff --git a/x b/x
@@ -40,0 +41 @@ fn main() {
+// new line
@@ -100,3 +100,3 @@ fn f() {
-old a
-old b
-old c
+new a
+new b
+new c
";
        let fd = parse_file_diff(diff);
        assert!(fd.added.contains(&41), "added line 41: {:?}", fd.added);
        assert!(
            fd.added.contains(&100) && fd.added.contains(&101) && fd.added.contains(&102),
            "3 added lines: {:?}",
            fd.added
        );
        // The 3 removed lines anchor before the first new line of their hunk (100).
        let removed_100: Vec<&str> = fd
            .removed_before
            .get(&100)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            removed_100,
            ["old a", "old b", "old c"],
            "removed lines anchored before 100: {:?}",
            fd.removed_before
        );
    }

    #[test]
    fn file_diff_pure_deletion_keeps_red_lines() {
        // `+50,0` deletes 2 lines and adds none — they still show as red, anchored at 50.
        let diff = "@@ -50,2 +50,0 @@\n-gone a\n-gone b\n";
        let fd = parse_file_diff(diff);
        assert!(fd.added.is_empty(), "no green lines: {:?}", fd.added);
        let removed_50: Vec<&str> = fd
            .removed_before
            .get(&50)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            removed_50,
            ["gone a", "gone b"],
            "deleted lines shown red at 50: {:?}",
            fd.removed_before
        );
    }

    #[test]
    fn hunks_group_added_runs_and_deletions() {
        // Line 41 added (pure addition); 100-102 replaced 3 removed lines.
        let diff = "\
@@ -40,0 +41 @@
+// new line
@@ -100,3 +100,3 @@
-old a
-old b
-old c
+new a
+new b
+new c
";
        let hunks = parse_file_diff(diff).hunks();
        assert_eq!(hunks.len(), 2, "two blocks: {hunks:?}");
        // Pure addition at 41 → empty head_text (revert = delete the line).
        assert_eq!(hunks[0].cur_start, 41);
        assert_eq!(hunks[0].cur_end, 41);
        assert_eq!(hunks[0].head_text, "");
        // Replacement at 100-102 → head_text is the removed committed lines.
        assert_eq!(hunks[1].cur_start, 100);
        assert_eq!(hunks[1].cur_end, 102);
        assert_eq!(hunks[1].head_text, "old a\nold b\nold c");
    }

    #[test]
    fn hunks_pure_deletion_has_empty_current_range() {
        let diff = "@@ -50,2 +49,0 @@\n-gone a\n-gone b\n";
        let hunks = parse_file_diff(diff).hunks();
        assert_eq!(hunks.len(), 1);
        // Empty current range (cur_end < cur_start) → revert re-inserts before cur_start.
        assert!(hunks[0].cur_end < hunks[0].cur_start, "empty range: {hunks:?}");
        assert_eq!(hunks[0].head_text, "gone a\ngone b");
    }

    #[test]
    fn a_pure_deletion_marks_no_current_lines() {
        // `+50,0` = nothing added on the new side (a deletion) → no highlighted lines.
        let diff = "@@ -50,2 +50,0 @@\n-gone a\n-gone b\n";
        assert!(
            changed_lines(diff).is_empty(),
            "deletion highlights nothing"
        );
    }

    #[test]
    fn empty_or_clean_diff_is_empty() {
        assert!(changed_lines("").is_empty());
        assert!(changed_lines("diff --git a/x b/x\n").is_empty());
    }

    #[test]
    fn comment_only_change_is_detected() {
        // Only doc-comment lines changed → safe to skip cargo check.
        let diff = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,2 +1,2 @@
-/// Old long comment that goes on
-/// and on and on.
+/// Short comment.
+
";
        assert!(is_comment_only_change(diff), "doc-comment-only: {diff}");
    }

    #[test]
    fn code_change_is_not_comment_only() {
        let diff = "\
--- a/x
+++ b/x
@@ -10 +10 @@
-let x = 1;
+let x = 2;
";
        assert!(
            !is_comment_only_change(diff),
            "real code change must verify"
        );
    }

    #[test]
    fn code_with_a_trailing_comment_is_not_comment_only() {
        // `let x = 2; // note` is a CODE change, not comment-only.
        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n+let x = 2; // note\n";
        assert!(!is_comment_only_change(diff));
    }

    #[test]
    fn no_content_change_is_not_skippable() {
        // An empty diff means nothing to skip verify for → false (don't claim comment-only).
        assert!(!is_comment_only_change(""));
        assert!(!is_comment_only_change("diff --git a/x b/x\n@@ -1 +1 @@\n"));
    }

    #[test]
    fn block_comment_lines_count_as_comments() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n+/* a block\n+ * comment\n+ */\n";
        assert!(is_comment_only_change(diff));
    }

    #[test]
    fn parse_status_maps_added_modified_deleted() {
        let porcelain = "\
?? new.rs
 M edited.rs
 D gone.rs
R  old.rs -> renamed.rs
";
        let m = parse_status(porcelain);
        assert_eq!(m.get("new.rs"), Some(&FileStatus::Added));
        assert_eq!(m.get("edited.rs"), Some(&FileStatus::Modified));
        assert_eq!(m.get("gone.rs"), Some(&FileStatus::Deleted));
        assert_eq!(
            m.get("renamed.rs"),
            Some(&FileStatus::Modified),
            "rename → new path"
        );
        assert_eq!(FileStatus::Added.badge(), "A");
    }

    #[test]
    fn parse_stage_states_splits_index_and_worktree() {
        let porcelain = "\
M  staged_only.rs
 M unstaged_only.rs
MM staged_and_more.rs
?? untracked.rs
A  added_staged.rs
D  removed_staged.rs
";
        let m = parse_stage_states(porcelain);
        // `M ` — staged, nothing further in the worktree.
        assert_eq!(m["staged_only.rs"], StageState { staged: true, unstaged: false });
        // ` M` — unstaged working-tree change only.
        assert_eq!(m["unstaged_only.rs"], StageState { staged: false, unstaged: true });
        // `MM` — staged AND further unstaged edits → shows in both sections.
        assert_eq!(m["staged_and_more.rs"], StageState { staged: true, unstaged: true });
        // `??` — untracked → fully unstaged.
        assert_eq!(m["untracked.rs"], StageState { staged: false, unstaged: true });
        // `A ` / `D ` — staged add / delete.
        assert!(m["added_staged.rs"].staged && !m["added_staged.rs"].unstaged);
        assert!(m["removed_staged.rs"].staged);
    }

    #[test]
    fn parse_numstat_counts_and_skips_binary() {
        let numstat = "\
12\t3\tsrc/main.rs
0\t7\tsrc/gone.rs
-\t-\tassets/logo.png
5\t2\tsrc/dir/{a => b}/x.rs
";
        let m = parse_numstat(numstat);
        assert_eq!(m["src/main.rs"], LineDelta { added: 12, removed: 3 });
        assert_eq!(m["src/gone.rs"], LineDelta { added: 0, removed: 7 });
        assert!(!m.contains_key("assets/logo.png"), "binary file skipped");
        // Rename maps to the resulting path.
        assert!(
            m.keys().any(|k| k.ends_with("x.rs")),
            "rename kept: {:?}",
            m.keys().collect::<Vec<_>>()
        );
    }
}
