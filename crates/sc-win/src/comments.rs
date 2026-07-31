//! Persisted inline code comments (the PR-review kind): each anchored to a file + line range,
//! carrying its text and whether the agent has resolved it. Stored in `.dc/comments.json` at
//! the project root so they survive restarts; the running list of resolved comments doubles as
//! a changelog for the eventual commit.
//!
//! Pure logic + JSON (via `serde_json`, already a dep). No iced types; the app renders these
//! inline in the code view and flips `resolved` when a fix lands.

use std::path::Path;

/// One inline comment, anchored to a line range of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Workspace-relative file path.
    pub file: String,
    /// 1-based first/last line the comment covers (inclusive). Single line ⇒ `start == end`.
    pub start: usize,
    pub end: usize,
    /// What the reviewer wrote.
    pub text: String,
    /// True once the agent has made the requested change.
    pub resolved: bool,
    /// The exact text of the range BEFORE the fix (captured when the change is applied), so a
    /// per-comment Revert can splice the original lines back. `None` until a fix resolves it.
    pub before: Option<String>,
    /// The number of lines the replacement produced (so Revert knows what range to swap back).
    /// `None` until resolved.
    pub after_len: Option<usize>,
}

impl Comment {
    pub fn new(file: impl Into<String>, start: usize, end: usize, text: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            start,
            end,
            text: text.into(),
            resolved: false,
            before: None,
            after_len: None,
        }
    }
}

/// The in-memory set of comments for a project, loaded from / saved to `.dc/comments.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comments {
    pub items: Vec<Comment>,
}

impl Comments {
    /// Add a comment (pending). Returns its index.
    pub fn add(&mut self, c: Comment) -> usize {
        self.items.push(c);
        self.items.len() - 1
    }

    /// Mark the most recent PENDING comment on `file` as resolved (the one a just-finished fix
    /// addressed). Returns true if one was found. Newest-first so re-commenting the same file
    /// resolves the latest ask.
    pub fn resolve_latest_on(&mut self, file: &str) -> bool {
        if let Some(c) = self
            .items
            .iter_mut()
            .rev()
            .find(|c| c.file == file && !c.resolved)
        {
            c.resolved = true;
            true
        } else {
            false
        }
    }

    /// Comments on `file`, in order (for rendering inline under their lines).
    pub fn on_file<'a>(&'a self, file: &'a str) -> impl Iterator<Item = (usize, &'a Comment)> {
        self.items
            .iter()
            .enumerate()
            .filter(move |(_, c)| c.file == file)
    }

    /// Remove the comment at index `i` (a manual dismiss). No-op if out of range.
    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            self.items.remove(i);
        }
    }
}

/// Format the pending comments on a phase's artifact file into send-back notes — the
/// code-review path to workflow feedback: a human opens a gating phase's `.md` in the code
/// view, drops line comments on the parts they want changed, and clicks "Send back"; those
/// comments BECOME the notes the workflow re-plans from.
///
/// The formatting itself is the engine's ([`sc_workflow::format_sendback_notes`]) so the CLI
/// can produce identical feedback; this only projects the GUI's richer [`Comment`] (which also
/// carries resolution + undo state) onto the engine's [`sc_workflow::ReviewNote`]. `comments`
/// should already be filtered to the phase's file.
pub fn format_sendback_notes(comments: &[&Comment]) -> Option<String> {
    let notes: Vec<sc_workflow::ReviewNote<'_>> = comments
        .iter()
        .map(|c| sc_workflow::ReviewNote::new(c.start, c.end, &c.text))
        .collect();
    sc_workflow::format_sendback_notes(&notes)
}

/// A phase's artifact file plus the comments left on it — one entry of a send-back harvest.
/// `phase` is the workflow phase the file belongs to; `file` is its workspace-relative path.
pub struct PhaseComments<'a> {
    pub phase: sc_workflow::Phase,
    pub file: &'a str,
    pub notes: Vec<&'a Comment>,
}

/// Resolve a send-back from the line comments across ALL phase artifacts — the GUI's
/// upstream send-back.
///
/// The reviewer reads a phase's `.md` and drops comments on it; the FILE a comment sits on
/// says which phase it's about, so a comment on `architecture.md` while the layout is gating
/// is unambiguously a request to change the architecture. Returns the target phase and the
/// formatted notes:
///
/// * The **target is the earliest commented phase** — [`sc_workflow::WorkflowState::invalidate_from`]
///   drops it *and everything downstream*, so any later commented phase is regenerating anyway
///   and bouncing to the earliest is what makes all the feedback apply.
/// * Notes from every commented phase are included, grouped under a `## <Phase>` header so the
///   model knows which artifact each bullet is about. Downstream comments are the *consequences*
///   the reviewer spotted while reading — the regeneration should see them.
/// * `None` when nothing is commented, so the caller falls back to the free-text box and the
///   gating phase.
///
/// `by_phase` should list every phase artifact with its comments, in pipeline order. Pure — no
/// I/O, so the targeting rule is host-testable.
pub fn resolve_sendback(
    by_phase: &[PhaseComments<'_>],
) -> Option<(sc_workflow::Phase, Option<String>)> {
    // Pipeline order, so "first commented" is "earliest phase".
    let mut commented: Vec<&PhaseComments<'_>> =
        by_phase.iter().filter(|p| !p.notes.is_empty()).collect();
    commented.sort_by_key(|p| p.phase.index());
    let target = commented.first()?.phase;

    // One section per commented phase. With a single phase commented (the common case) we skip
    // the header entirely — the notes read exactly as they did before upstream send-back existed.
    let notes = if commented.len() == 1 {
        format_sendback_notes(&commented[0].notes)
    } else {
        let sections: Vec<String> = commented
            .iter()
            .filter_map(|p| {
                format_sendback_notes(&p.notes).map(|b| format!("## {}\n{b}", p.phase.title()))
            })
            .collect();
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    };
    Some((target, notes))
}

/// The `.dc/comments.json` path under a project root.
fn store_path(root: &Path) -> std::path::PathBuf {
    root.join(".dc").join("comments.json")
}

/// Load the comments for a project (empty if none / unreadable).
pub fn load(root: &Path) -> Comments {
    match std::fs::read_to_string(store_path(root)) {
        Ok(text) => parse(&text),
        Err(_) => Comments::default(),
    }
}

/// Persist the comments to `.dc/comments.json` (best-effort; creates `.dc/` as needed).
pub fn save(root: &Path, comments: &Comments) {
    let dir = root.join(".dc");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(store_path(root), serialize(comments));
}

/// Serialize to JSON (manual, to avoid deriving Serialize across the small struct).
fn serialize(c: &Comments) -> String {
    let arr: Vec<serde_json::Value> = c
        .items
        .iter()
        .map(|c| {
            serde_json::json!({
                "file": c.file,
                "start": c.start,
                "end": c.end,
                "text": c.text,
                "resolved": c.resolved,
                "before": c.before,
                "after_len": c.after_len,
            })
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}

/// Parse the JSON produced by [`serialize`]. Tolerant: skips malformed entries.
fn parse(text: &str) -> Comments {
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(text) else {
        return Comments::default();
    };
    let items = arr
        .iter()
        .filter_map(|v| {
            Some(Comment {
                file: v.get("file")?.as_str()?.to_string(),
                start: v.get("start")?.as_u64()? as usize,
                end: v.get("end")?.as_u64()? as usize,
                text: v.get("text")?.as_str()?.to_string(),
                resolved: v.get("resolved").and_then(|r| r.as_bool()).unwrap_or(false),
                before: v.get("before").and_then(|b| b.as_str()).map(str::to_string),
                after_len: v
                    .get("after_len")
                    .and_then(|a| a.as_u64())
                    .map(|n| n as usize),
            })
        })
        .collect();
    Comments { items }
}

/// Ensure `.dc/` is git-ignored: if the project has a `.gitignore` without a `.dc/` entry (or
/// none at all), append one. Called on project open so the store never gets committed. Returns
/// true if it wrote/updated the file.
pub fn ensure_gitignored(root: &Path) -> bool {
    let gi = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == ".dc/" || l.trim() == ".dc")
    {
        return false; // already ignored
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".dc/\n");
    std::fs::write(&gi, updated).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_comments() {
        let mut c = Comments::default();
        c.add(Comment::new("a.rs", 1, 3, "shorten this"));
        c.add(Comment::new("b.rs", 10, 10, "rename x"));
        c.items[0].resolved = true;
        let back = parse(&serialize(&c));
        assert_eq!(back, c);
    }

    #[test]
    fn resolve_latest_marks_the_newest_pending_on_a_file() {
        let mut c = Comments::default();
        c.add(Comment::new("a.rs", 1, 1, "first"));
        c.add(Comment::new("a.rs", 5, 5, "second"));
        assert!(c.resolve_latest_on("a.rs"));
        // The SECOND (newest) got resolved, not the first.
        assert!(!c.items[0].resolved && c.items[1].resolved);
        // Resolving again gets the first.
        assert!(c.resolve_latest_on("a.rs"));
        assert!(c.items[0].resolved);
        // Nothing left pending → false.
        assert!(!c.resolve_latest_on("a.rs"));
    }

    #[test]
    fn on_file_filters_by_path() {
        let mut c = Comments::default();
        c.add(Comment::new("a.rs", 1, 1, "x"));
        c.add(Comment::new("b.rs", 1, 1, "y"));
        c.add(Comment::new("a.rs", 2, 2, "z"));
        let got: Vec<&str> = c.on_file("a.rs").map(|(_, c)| c.text.as_str()).collect();
        assert_eq!(got, vec!["x", "z"]);
    }

    #[test]
    fn format_sendback_notes_bullets_each_comment_with_its_range() {
        let a = Comment::new("specs/x/spec.md", 3, 3, "tighten the goal");
        let b = Comment::new("specs/x/spec.md", 10, 14, "this section is out of scope");
        let notes = format_sendback_notes(&[&a, &b]).unwrap();
        assert_eq!(
            notes,
            "- [line 3] tighten the goal\n- [lines 10-14] this section is out of scope"
        );
    }

    #[test]
    fn format_sendback_notes_none_when_no_comments() {
        assert_eq!(format_sendback_notes(&[]), None);
    }

    use sc_workflow::Phase;

    /// Build a `PhaseComments` list from (phase, file, comments) triples.
    fn by_phase<'a>(rows: &'a [(Phase, &'a str, Vec<&'a Comment>)]) -> Vec<PhaseComments<'a>> {
        rows.iter()
            .map(|(phase, file, notes)| PhaseComments {
                phase: *phase,
                file,
                notes: notes.clone(),
            })
            .collect()
    }

    #[test]
    fn resolve_sendback_targets_the_gating_phase_when_only_it_is_commented() {
        // The pre-existing behavior: comments on the phase being reviewed bounce it to itself,
        // and the notes are the plain bullet list with no phase header.
        let c = Comment::new("specs/x/layout.md", 4, 4, "split this module");
        let rows = [
            (Phase::Architecture, "specs/x/architecture.md", vec![]),
            (Phase::Layout, "specs/x/layout.md", vec![&c]),
        ];
        let (target, notes) = resolve_sendback(&by_phase(&rows)).unwrap();
        assert_eq!(target, Phase::Layout);
        assert_eq!(notes.as_deref(), Some("- [line 4] split this module"));
    }

    #[test]
    fn resolve_sendback_targets_the_earliest_commented_phase() {
        // The upstream case: reading the layout, the reviewer realises the ARCHITECTURE is
        // wrong. A comment on architecture.md must bounce to Architecture — invalidate_from
        // then drops layout too, so it regenerates from the corrected architecture.
        let arch = Comment::new("specs/x/architecture.md", 7, 9, "events, not polling");
        let layout = Comment::new("specs/x/layout.md", 2, 2, "follows from the above");
        let rows = [
            (Phase::Architecture, "specs/x/architecture.md", vec![&arch]),
            (Phase::Layout, "specs/x/layout.md", vec![&layout]),
        ];
        let (target, notes) = resolve_sendback(&by_phase(&rows)).unwrap();
        assert_eq!(target, Phase::Architecture, "earliest commented phase wins");

        // Both phases' notes ride along, grouped so the model knows which artifact each is
        // about — the downstream comment is the consequence the reviewer already spotted.
        let notes = notes.unwrap();
        assert_eq!(
            notes,
            "## Architecture\n- [lines 7-9] events, not polling\n\n## Layout\n- [line 2] follows from the above"
        );
    }

    #[test]
    fn resolve_sendback_is_none_when_nothing_is_commented() {
        // No comments anywhere → the caller falls back to the free-text note box and the
        // gating phase, exactly as before.
        let rows = [
            (Phase::Specs, "specs/x/spec.md", vec![]),
            (Phase::Layout, "specs/x/layout.md", vec![]),
        ];
        assert!(resolve_sendback(&by_phase(&rows)).is_none());
        assert!(resolve_sendback(&[]).is_none());
    }

    #[test]
    fn resolve_sendback_orders_by_pipeline_not_by_input_order() {
        // The caller may hand rows in any order; targeting must follow the PIPELINE order.
        let spec = Comment::new("specs/x/spec.md", 1, 1, "wrong goal");
        let layout = Comment::new("specs/x/layout.md", 5, 5, "and so this is wrong");
        let rows = [
            (Phase::Layout, "specs/x/layout.md", vec![&layout]),
            (Phase::Specs, "specs/x/spec.md", vec![&spec]),
        ];
        let (target, notes) = resolve_sendback(&by_phase(&rows)).unwrap();
        assert_eq!(target, Phase::Specs);
        // Sections are in pipeline order too, so the model reads cause before consequence.
        let notes = notes.unwrap();
        assert!(
            notes.find("## Specs").unwrap() < notes.find("## Layout").unwrap(),
            "sections in pipeline order: {notes}"
        );
    }

    #[test]
    fn ensure_gitignored_appends_dc() {
        let dir = std::env::temp_dir().join(format!("dc-gi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No .gitignore yet → creates one with .dc/.
        assert!(ensure_gitignored(&dir));
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(gi.contains(".dc/"));
        // Idempotent: already ignored → no rewrite.
        assert!(!ensure_gitignored(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_gitignored_preserves_existing_entries() {
        let dir = std::env::temp_dir().join(format!("dc-gi2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "/target\n/screenshots\n").unwrap();
        assert!(ensure_gitignored(&dir));
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            gi.contains("/target") && gi.contains(".dc/"),
            "kept existing + added: {gi}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
