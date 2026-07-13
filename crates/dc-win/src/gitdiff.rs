//! Live git-diff line tracking for the code view: which lines of a file differ from its
//! committed (HEAD) state right now. Used to highlight changed lines GitHub-PR-style AS the
//! agent edits — ground truth from git, not inferred from events.
//!
//! The hunk-header parsing is pure/host-testable; the app runs `git diff -U0` each tick and
//! feeds the output here.

use std::collections::BTreeSet;
use std::path::Path;

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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["diff", "-U0", "--no-color", "--", rel])
        .output();
    match out {
        Ok(o) if o.status.success() => changed_lines(&String::from_utf8_lossy(&o.stdout)),
        _ => BTreeSet::new(),
    }
}

/// The raw `git diff` for `files` in `workspace` (with content lines), for content analysis
/// like [`is_comment_only_change`]. Empty string if git isn't present / nothing changed.
pub fn files_diff(workspace: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let out = std::process::Command::new("git")
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
}
