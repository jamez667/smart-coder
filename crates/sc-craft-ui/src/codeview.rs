//! The right-hand CODE panel model: reading a selected file into numbered, bounded
//! display lines.
//!
//! Pure logic, no iced types, host-testable. Both products call [`load`] for the
//! display lines.
//!
//! **"Follow the agent"** — deciding which file to show as the agent works — used to
//! live here too. It reads `sc_core::AgentEvent`, so it moved to `sc_win::follow`
//! when the editor was split out (spec 21): this crate is in the Crafter's dependency
//! tree, and nothing model-shaped may be. [`normalize`] stayed, and is `pub` because
//! that half still needs it.

use std::path::Path;

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

/// A live preview extracted from a *partial* streamed tool call: the file being written and
/// the content produced so far. Used to show a `write_file`/`edit_file` appear word-by-word
/// in the code view before the call completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPreview {
    /// The target file path, if it's been emitted yet.
    pub file: Option<String>,
    /// The in-progress new content (the `new_str` / `content` / code-fence body so far).
    pub content: String,
}

/// Extract a live edit preview from the model's growing turn text `cumulative`. Handles the
/// two shapes a small coder emits: a JSON tool call (`"path": "...", "new_str"/"content": "…`)
/// and a plain code fence (```lang\n…). Returns `None` until there's a file path or content
/// worth showing. Best-effort and tolerant of half-written JSON — it just shows what's there.
pub fn partial_edit_preview(cumulative: &str) -> Option<EditPreview> {
    // The file path: first "path": "…" (may be unterminated).
    let file = json_string_after(cumulative, "\"path\"");
    // The content: whatever follows the LAST of new_str/content/append content key; if the
    // closing quote hasn't streamed yet we take the rest of the buffer (unescaped).
    let content = json_string_after_open(cumulative, "\"new_str\"")
        .or_else(|| json_string_after_open(cumulative, "\"content\""))
        .or_else(|| code_fence_body(cumulative));
    match (file, content) {
        (None, None) => None,
        (file, content) => Some(EditPreview {
            file,
            content: content.unwrap_or_default(),
        }),
    }
}

/// The fully-quoted string value after `key` (`key" : "value"`), unescaped. `None` if the
/// value's closing quote hasn't arrived yet.
fn json_string_after(s: &str, key: &str) -> Option<String> {
    let (rest, _) = open_value(s, key)?;
    let end = find_unescaped_quote(rest)?;
    Some(unescape(&rest[..end]))
}

/// The string value after `key`, taking everything up to the closing quote OR the end of the
/// buffer if it hasn't streamed yet (so a still-writing value previews live). Unescaped.
fn json_string_after_open(s: &str, key: &str) -> Option<String> {
    let (rest, _) = open_value(s, key)?;
    let body = match find_unescaped_quote(rest) {
        Some(end) => &rest[..end],
        None => rest, // value still streaming
    };
    Some(unescape(body))
}

/// Advance past `key`, its `:`, and the opening quote of the value; return the remainder
/// (the value's body, possibly unterminated) — the LAST occurrence of `key` wins so a retry
/// or later field is what we preview.
fn open_value<'a>(s: &'a str, key: &str) -> Option<(&'a str, usize)> {
    let idx = s.rfind(key)?;
    let after = &s[idx + key.len()..];
    let colon = after.find(':')?;
    let after_colon = &after[colon + 1..];
    let q = after_colon.find('"')?;
    Some((&after_colon[q + 1..], idx))
}

/// Index of the first unescaped `"` in `s` (the JSON string terminator).
fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped char
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Unescape the common JSON string escapes a model emits (`\n \t \" \\`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The body of a plain code fence (```lang\n…), for a model that "thinks out loud" and writes
/// the file as a fenced block instead of JSON. Takes everything after the opening fence's
/// newline, up to a closing ``` or the end of the buffer.
fn code_fence_body(s: &str) -> Option<String> {
    let open = s.find("```")?;
    let after = &s[open + 3..];
    let nl = after.find('\n')?; // skip the info string line
    let body = &after[nl + 1..];
    let end = body.find("```").unwrap_or(body.len());
    let body = &body[..end];
    if body.trim().is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// Normalize a path to a workspace-relative, forward-slashed form. A stray leading
/// `./` or backslashes shouldn't defeat a tree/highlight match.
///
/// `pub` for `sc_win::follow`, which normalizes tool-call path args with it.
pub fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_numbers_lines_from_one() {
        let dir = std::env::temp_dir().join(format!("sc-win-cv-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("sc-win-cv-miss-{}", std::process::id()));
        let cv = load(&dir, "nope.rs");
        assert!(cv.lines.is_empty());
        assert_eq!(cv.note.as_deref(), Some("(file not found)"));
    }

    #[test]
    fn binary_file_is_flagged() {
        let dir = std::env::temp_dir().join(format!("sc-win-cv-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        let cv = load(&dir, "blob.bin");
        assert_eq!(cv.note.as_deref(), Some("(binary file — not shown)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_edit_preview_extracts_growing_content() {
        // A write_file JSON whose content is still streaming (no closing quote yet).
        let partial = r#"{"tool":"write_file","path":"src/a.rs","content":"fn main() {\n    let x"#;
        let p = partial_edit_preview(partial).unwrap();
        assert_eq!(p.file.as_deref(), Some("src/a.rs"));
        assert_eq!(p.content, "fn main() {\n    let x", "unescaped, live");
    }

    #[test]
    fn partial_edit_preview_handles_new_str_and_completed_value() {
        let done = r#"{"tool":"edit_file","path":"x.rs","old_str":"a","new_str":"b\nc"}"#;
        let p = partial_edit_preview(done).unwrap();
        assert_eq!(p.file.as_deref(), Some("x.rs"));
        assert_eq!(p.content, "b\nc");
    }

    #[test]
    fn partial_edit_preview_reads_a_code_fence() {
        let fence = "Sure:\n```rust\nfn main() {}\n";
        let p = partial_edit_preview(fence).unwrap();
        assert!(p.content.contains("fn main() {}"), "{:?}", p.content);
    }

    #[test]
    fn partial_edit_preview_none_before_anything_useful() {
        assert!(partial_edit_preview("{\"tool\":\"wri").is_none());
        assert!(partial_edit_preview("").is_none());
    }
}
