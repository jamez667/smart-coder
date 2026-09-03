//! Previous Claude Code conversations, read from its own on-disk session logs.
//!
//! The CLI stores one JSONL per conversation under
//! `~/.claude/projects/<slugged-workspace-path>/<session-id>.jsonl`, and `--resume
//! <id>` reopens one. `--continue` (which the panel already offers) silently takes
//! the most recent; a *picker* needs to know what the others are, which means
//! reading those files.
//!
//! Deliberately reads the logs rather than shelling out to `--resume` with no id:
//! that opens the CLI's own interactive TUI picker, and `sc-win` captures stdout
//! for its feed instead of attaching a terminal, so the picker would have nothing
//! to draw on and nothing to read keystrokes from.
//!
//! Everything here is best-effort. A missing directory, an unreadable file, a
//! half-written line — the session simply does not appear. A picker that refuses to
//! open because one log is malformed is worse than one that lists nine of ten.

use std::path::{Path, PathBuf};

/// One resumable conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The CLI's session id — the filename stem, and what `--resume` takes.
    pub id: String,
    /// The first thing the user asked, trimmed to one line. The only label that
    /// tells you which conversation this was.
    pub summary: String,
    /// Unix seconds of last modification, for ordering. Newest first is what a
    /// picker wants; "which did I use last" is the question being asked.
    pub modified: u64,
}

/// Where the CLI keeps this workspace's conversations.
///
/// The directory name is the absolute path with every non-alphanumeric character
/// replaced by `-`, case preserved: `C:\Users\mail\ws` becomes
/// `C--Users-mail-ws`. Derived rather than searched because the same workspace must
/// map to the same directory every time.
pub fn project_dir(workspace: &Path) -> Option<PathBuf> {
    let home = home_dir()?;
    // The drive letter is upper-cased: the CLI writes `C--Users-...`, and while
    // Windows resolves either spelling, a case-sensitive mount would not.
    let raw = workspace.to_string_lossy();
    let mut slug: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if let Some(first) = slug.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some(home.join(".claude").join("projects").join(slug))
}

/// The user's home directory, without pulling in a crate for it.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Every resumable conversation for `workspace`, newest first.
///
/// Empty when the CLI has never run here, which is the honest answer — the picker
/// then says there is nothing to resume rather than showing an error.
pub fn list(workspace: &Path) -> Vec<Session> {
    let Some(dir) = project_dir(workspace) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out: Vec<Session> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let modified = e
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            // A session opened from an IDE context block can genuinely have no plain
            // user prompt in its opening lines. "(no prompt)" is honest but useless
            // in a picker, so fall back to the short id -- which at least tells two
            // unlabelled rows apart, and is what `--resume` takes anyway.
            let summary = first_user_message(&path)
                .unwrap_or_else(|| format!("(session {})", &id[..id.len().min(8)]));
            Some(Session {
                summary,
                id,
                modified,
            })
        })
        .collect();

    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// The first real thing the user typed in a session log.
///
/// Skips the machinery: a session opens with tool results, system reminders and
/// resumed-context blocks, none of which identify the conversation. The first line
/// that is genuinely a person asking something is the only useful label, so lines
/// that start with `<` (the `<system-reminder>` / `<command-name>` wrappers) are
/// passed over.
fn first_user_message(path: &Path) -> Option<String> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    // Streamed, not read whole: these logs run to tens of megabytes and the answer
    // is almost always in the first few lines.
    for line in std::io::BufReader::new(file).lines().take(400) {
        let Ok(line) = line else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = v.get("message")?.get("content")?;
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            _ => continue,
        };
        let text = text.trim();
        // Skip the machinery. A session commonly opens with `<local-command-caveat>`,
        // `<command-name>/clear</command-name>`, or an `<ide_opened_file>` block --
        // and the last of those arrives inside the ARRAY form's text part, so
        // checking the raw string alone missed it and a 22MB conversation showed as
        // "(no prompt)".
        if text.is_empty() || text.starts_with('<') {
            continue;
        }
        return Some(one_line(text, 72));
    }
    None
}

/// One rendered line of a past conversation.
///
/// Deliberately not [`crate::view::Row`]: this module knows nothing about the UI,
/// so the caller maps these into whatever it draws with. `is_user` is the only
/// distinction the feed needs — a question reads differently from an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub is_user: bool,
    pub text: String,
}

/// How many lines of a past conversation to replay into the feed.
///
/// These logs reach tens of megabytes. Replaying one whole would stall the UI and
/// bury the thing you resumed for; the recent exchange is what tells you where you
/// left off.
const REPLAY_LINES: usize = 60;

/// The tail of a session, as lines to show in the feed.
///
/// Assistant text only — tool calls, results and thinking blocks are the machinery
/// of how it got there, and a resumed panel wants the conversation, not the
/// transcript. Returns oldest-first so the caller can push straight into a feed.
pub fn transcript(workspace: &Path, id: &str) -> Vec<TranscriptLine> {
    use std::io::BufRead;

    let Some(dir) = project_dir(workspace) else {
        return Vec::new();
    };
    let path = dir.join(format!("{id}.jsonl"));
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };

    let mut all: Vec<TranscriptLine> = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let is_user = match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => true,
            Some("assistant") => false,
            _ => continue,
        };
        let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        let text = text.trim();
        // The same machinery `first_user_message` skips, for the same reason.
        if text.is_empty() || text.starts_with('<') {
            continue;
        }
        all.push(TranscriptLine {
            is_user,
            text: one_line(text, 160),
        });
    }

    // The TAIL: where you left off, not where you began.
    let start = all.len().saturating_sub(REPLAY_LINES);
    all.split_off(start)
}

/// Collapse to a single line and clip, so a row cannot wrap the menu.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_slug_replaces_every_non_alphanumeric() {
        let dir = project_dir(Path::new(r"C:\Users\mail\ws")).expect("home");
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        // Case is preserved: the CLI's own directory is `C--Users-...`, not `c--`.
        assert_eq!(name, "C--Users-mail-ws");
        assert!(dir.ends_with(Path::new(".claude/projects/C--Users-mail-ws")) || cfg!(windows));
    }

    #[test]
    fn a_workspace_the_cli_has_never_seen_lists_nothing() {
        let never = std::env::temp_dir().join("sc-win-no-such-workspace-xyz");
        assert!(list(&never).is_empty(), "must not error, just be empty");
    }

    #[test]
    fn one_line_collapses_and_clips() {
        assert_eq!(one_line("hello   world", 40), "hello world");
        assert_eq!(one_line("a\nb\tc", 40), "a b c");
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
        // Already short enough: returned whole, no ellipsis.
        assert_eq!(one_line("short", 40), "short");
    }

    /// The summary must be the user's QUESTION, not the machinery a session opens
    /// with — a picker listing ten rows of `<system-reminder>` identifies nothing.
    #[test]
    fn the_summary_skips_wrappers_and_finds_the_real_prompt() {
        let dir = std::env::temp_dir().join(format!("sc-win-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("abc123.jsonl");
        std::fs::write(
            &log,
            concat!(
                r#"{"type":"user","message":{"content":"<system-reminder>noise</system-reminder>"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":"hi"}}"#,
                "\n",
                r#"{"type":"user","message":{"content":[{"type":"text","text":"fix the parser"}]}}"#,
                "\n",
            ),
        )
        .unwrap();

        assert_eq!(
            first_user_message(&log).as_deref(),
            Some("fix the parser"),
            "the wrapper line must be skipped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The replay is the CONVERSATION, not the machinery, and it is the TAIL.
    #[test]
    fn the_transcript_replays_the_tail_of_the_exchange() {
        let ws = std::env::temp_dir().join(format!("sc-win-tr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        // Build the log where `project_dir` will look for it.
        let dir = project_dir(&ws).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        body.push_str(
            "{\"type\":\"user\",\"message\":{\"content\":\"<system-reminder>x</system-reminder>\"}}\n",
        );
        body.push_str("{\"type\":\"user\",\"message\":{\"content\":\"first question\"}}\n");
        body.push_str("{\"type\":\"assistant\",\"message\":{\"content\":\"an answer\"}}\n");
        std::fs::write(dir.join("sess1.jsonl"), body).unwrap();

        let lines = transcript(&ws, "sess1");
        assert_eq!(
            lines,
            vec![
                TranscriptLine {
                    is_user: true,
                    text: "first question".into()
                },
                TranscriptLine {
                    is_user: false,
                    text: "an answer".into()
                },
            ],
            "wrappers skipped, roles kept, order preserved"
        );

        // An id with no log is empty, not a panic.
        assert!(transcript(&ws, "nope").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A malformed log must not take the picker down with it.
    #[test]
    fn a_broken_log_yields_no_summary_rather_than_panicking() {
        let dir = std::env::temp_dir().join(format!("sc-win-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("bad.jsonl");
        std::fs::write(&log, "not json at all\n{\"half\": \n").unwrap();

        assert_eq!(first_user_message(&log), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
