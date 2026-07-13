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
}

impl Comment {
    pub fn new(file: impl Into<String>, start: usize, end: usize, text: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            start,
            end,
            text: text.into(),
            resolved: false,
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
