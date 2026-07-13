//! The right-hand CODE panel model: reading a selected file into numbered, bounded
//! display lines, and deciding *which* file to show as the agent works
//! ("follow the agent" — the code pane auto-jumps to the file being edited).
//!
//! Pure logic, no iced types, host-testable. `app.rs` calls [`load`] for the display
//! lines and [`file_touched_by`] to follow the event stream.

use std::path::Path;

use dc_core::AgentEvent;

/// A rendered file for the code viewer: numbered lines plus a header path. Capped so a
/// giant generated file can't blow the render / memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeView {
    /// Workspace-relative path shown in the panel header.
    pub rel: String,
    /// `(line_number, text)` pairs, 1-indexed, capped at [`MAX_LINES`].
    pub lines: Vec<(usize, String)>,
    /// True when the file was longer than [`MAX_LINES`] and got truncated.
    pub truncated: bool,
    /// Set instead of `lines` when the file couldn't be shown (missing, binary, too
    /// big to read) — a short human note for the panel.
    pub note: Option<String>,
}

/// Max lines rendered in the viewer. Beyond this we truncate with a note — the panel is
/// for *watching changes land*, not reading a 10k-line file end to end.
pub const MAX_LINES: usize = 4000;

/// Load `rel` (workspace-relative) under `root` into a [`CodeView`]. Never panics:
/// a missing file, a binary blob, or a read error yields a `note` instead of lines.
pub fn load(root: &Path, rel: &str) -> CodeView {
    let path = root.join(rel);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            return CodeView {
                rel: rel.to_string(),
                lines: Vec::new(),
                truncated: false,
                note: Some("(file not found)".to_string()),
            }
        }
    };
    // Treat a NUL byte as the binary signal (matches what most editors do cheaply).
    if bytes.contains(&0) {
        return CodeView {
            rel: rel.to_string(),
            lines: Vec::new(),
            truncated: false,
            note: Some("(binary file — not shown)".to_string()),
        };
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<(usize, String)> = text
        .lines()
        .take(MAX_LINES)
        .enumerate()
        .map(|(i, l)| (i + 1, l.to_string()))
        .collect();
    let total = text.lines().count();
    let truncated = total > MAX_LINES;
    if lines.is_empty() {
        // An empty (or whitespace-only) file: show one placeholder row so the header
        // and an "empty" cue render rather than a blank void.
        lines.push((1, String::new()));
    }
    CodeView {
        rel: rel.to_string(),
        lines,
        truncated,
        note: None,
    }
}

/// Render arbitrary in-memory text into a [`CodeView`] (not read from disk) — used to show
/// a *proposed* plan-file's contents before it's written. Same numbering/cap rules as
/// [`load`]. `rel` is the label shown in the header.
pub fn from_text(rel: &str, text: &str) -> CodeView {
    let mut lines: Vec<(usize, String)> = text
        .lines()
        .take(MAX_LINES)
        .enumerate()
        .map(|(i, l)| (i + 1, l.to_string()))
        .collect();
    let truncated = text.lines().count() > MAX_LINES;
    if lines.is_empty() {
        lines.push((1, String::new()));
    }
    CodeView {
        rel: rel.to_string(),
        lines,
        truncated,
        note: None,
    }
}

/// If this event is a tool call that *touches a file*, return the workspace-relative
/// path it touched — so the code pane can follow the agent to it. Covers the file-bearing
/// tools (read/write/edit/create); returns `None` for everything else.
///
/// The `arg` on these events is the path the tool acted on (see `dc-cli`'s `print_event`
/// / the tool schemas): for `read_file`/`write_file`/`create_file`/`edit_file` it is the
/// file path. We surface writes/edits *and* reads: watching the agent read the file it's
/// about to change is part of "watch it work", and the next edit re-selects the same file.
pub fn file_touched_by(ev: &AgentEvent) -> Option<String> {
    if let AgentEvent::ToolCall { tool, arg } = ev {
        if matches!(
            tool.as_str(),
            "read_file" | "write_file" | "create_file" | "edit_file"
        ) {
            let path = arg.trim();
            if !path.is_empty() {
                return Some(normalize(path));
            }
        }
    }
    None
}

/// Whether a touched file is an *edit/write* (a real change) vs. a mere read — the pane
/// prefers to pin to files being changed, but falls back to reads when nothing's been
/// edited yet.
pub fn is_mutating_touch(ev: &AgentEvent) -> bool {
    matches!(
        ev,
        AgentEvent::ToolCall { tool, .. }
            if matches!(tool.as_str(), "write_file" | "create_file" | "edit_file")
    )
}

/// Normalize a tool's path arg to a workspace-relative, forward-slashed form. Tool args
/// are already workspace-relative in practice, but a stray leading `./` or backslashes
/// shouldn't defeat the tree/highlight match.
fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_numbers_lines_from_one() {
        let dir = std::env::temp_dir().join(format!("dc-win-cv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        let cv = load(&dir, "a.rs");
        assert_eq!(cv.rel, "a.rs");
        assert_eq!(cv.lines[0], (1, "fn main() {}".to_string()));
        assert_eq!(cv.lines[1], (2, "let x = 1;".to_string()));
        assert!(!cv.truncated);
        assert!(cv.note.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_a_note_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("dc-win-cv-miss-{}", std::process::id()));
        let cv = load(&dir, "nope.rs");
        assert!(cv.lines.is_empty());
        assert_eq!(cv.note.as_deref(), Some("(file not found)"));
    }

    #[test]
    fn binary_file_is_flagged() {
        let dir = std::env::temp_dir().join(format!("dc-win-cv-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        let cv = load(&dir, "blob.bin");
        assert_eq!(cv.note.as_deref(), Some("(binary file — not shown)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_and_read_calls_select_their_file_others_dont() {
        let edit = AgentEvent::ToolCall {
            tool: "edit_file".to_string(),
            arg: "crates/city/src/sim.rs".to_string(),
        };
        assert_eq!(
            file_touched_by(&edit).as_deref(),
            Some("crates/city/src/sim.rs")
        );
        assert!(is_mutating_touch(&edit));

        let read = AgentEvent::ToolCall {
            tool: "read_file".to_string(),
            arg: "./crates/city/src/main.rs".to_string(),
        };
        assert_eq!(
            file_touched_by(&read).as_deref(),
            Some("crates/city/src/main.rs"),
            "leading ./ normalized"
        );
        assert!(!is_mutating_touch(&read), "a read is not a mutation");

        // A non-file tool selects nothing.
        let other = AgentEvent::ToolCall {
            tool: "run_verification".to_string(),
            arg: String::new(),
        };
        assert!(file_touched_by(&other).is_none());
    }
}
