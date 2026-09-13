//! "Follow the agent": deciding which file the CODE pane should show as the agent works.
//!
//! Split from [`sc_craft_ui::codeview`] when the editor became its own crate (spec 21).
//! The rendering half is shared by both products; this half reads `sc_core::AgentEvent`,
//! so it can only exist in the agent build — the Crafter has no event stream to follow
//! and no `sc-core` in its dependency tree.

use sc_core::AgentEvent;
use sc_craft_ui::codeview::normalize;

/// If this event is a tool call that *touches a file*, return the workspace-relative
/// path it touched — so the code pane can follow the agent to it. Covers the file-bearing
/// tools (read/write/edit/create); returns `None` for everything else.
///
/// The `arg` on these events is the path the tool acted on (see `sc-cli`'s `print_event`
/// / the tool schemas): for `read_file`/`write_file`/`create_file`/`edit_file` it is the
/// file path. We surface writes/edits *and* reads: watching the agent read the file it's
/// about to change is part of "watch it work", and the next edit re-selects the same file.
pub fn file_touched_by(ev: &AgentEvent) -> Option<String> {
    if let AgentEvent::ToolCall { tool, arg } = ev {
        if matches!(
            tool.as_str(),
            // smart-coder's own tools (spec 04) …
            "read_file" | "write_file" | "create_file" | "edit_file"
            // … and Claude Code's, which are PascalCase and a different vocabulary
            // entirely (spec 22). Without these a Claude Code run edits files and the
            // outcome banner reports zero, while the code pane follows nothing.
            | "Read" | "Write" | "Edit" | "NotebookEdit"
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
            if matches!(
                tool.as_str(),
                "write_file" | "create_file" | "edit_file"
                // Claude Code's writing tools (spec 22). `Read` is deliberately absent —
                // it is a touch but not a change, exactly like `read_file`.
                | "Write" | "Edit" | "NotebookEdit"
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// **Claude Code's tools count too** (spec 22).
    ///
    /// Its vocabulary is a different one — PascalCase `Edit`/`Write` rather than
    /// `edit_file`/`write_file`. Recognising only ours meant a Claude Code run edited files
    /// and the outcome banner reported zero while the code pane followed nothing: two silent
    /// failures, because an unmatched tool name simply matches nothing.
    #[test]
    fn claude_codes_tool_vocabulary_is_recognised_too() {
        for tool in ["Edit", "Write", "NotebookEdit"] {
            let ev = AgentEvent::ToolCall {
                tool: tool.to_string(),
                arg: "src/main.rs".to_string(),
            };
            assert_eq!(
                file_touched_by(&ev).as_deref(),
                Some("src/main.rs"),
                "{tool}"
            );
            assert!(is_mutating_touch(&ev), "{tool} writes, so it is a mutation");
        }

        // `Read` is a touch but NOT a mutation — the same distinction `read_file` draws.
        let read = AgentEvent::ToolCall {
            tool: "Read".to_string(),
            arg: "src/lib.rs".to_string(),
        };
        assert_eq!(file_touched_by(&read).as_deref(), Some("src/lib.rs"));
        assert!(!is_mutating_touch(&read), "reading is not changing");

        // A Claude Code tool that isn't about files selects nothing.
        let bash = AgentEvent::ToolCall {
            tool: "Bash".to_string(),
            arg: "cargo test".to_string(),
        };
        assert!(file_touched_by(&bash).is_none());
        assert!(!is_mutating_touch(&bash));
    }
}
