//! The minimal tool surface for the M0 agent loop (spec 04 — Tools).
//!
//! Deliberately tiny — a few narrow, strongly-typed tools beat a broad, ambiguous
//! surface for a small model. All paths are sandboxed to the workspace root.

use std::path::{Component, Path, PathBuf};

use dc_proto::{DcError, Result};
use serde::{Deserialize, Serialize};

/// One tool call the model can emit. Internally tagged JSON:
/// `{"tool":"write_file","path":"x","content":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum Tool {
    /// Read a file's contents.
    ReadFile { path: String },
    /// Write (create/overwrite) a file with the given full contents.
    WriteFile { path: String, content: String },
    /// Declare the task complete.
    Finish,
}

/// Result of executing a tool.
pub enum ToolOutcome {
    /// Text fed back to the model as the next observation.
    Observation(String),
    /// The model called `finish`.
    Finished,
}

/// Execute a tool against the workspace. Never fails: errors become observations
/// the model can react to (spec 04 — structured, actionable feedback).
pub fn execute(tool: &Tool, workspace: &Path) -> ToolOutcome {
    match tool {
        Tool::Finish => ToolOutcome::Finished,
        Tool::ReadFile { path } => ToolOutcome::Observation(match safe_join(workspace, path) {
            Ok(p) => match std::fs::read_to_string(&p) {
                Ok(c) => format!("read_file {path}:\n{c}"),
                Err(e) => format!("read_file {path} error: {e}"),
            },
            Err(e) => format!("read_file {path} rejected: {e}"),
        }),
        Tool::WriteFile { path, content } => {
            ToolOutcome::Observation(match safe_join(workspace, path) {
                Ok(p) => {
                    if let Some(parent) = p.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::write(&p, content) {
                        Ok(()) => format!("write_file {path} ok ({} bytes)", content.len()),
                        Err(e) => format!("write_file {path} error: {e}"),
                    }
                }
                Err(e) => format!("write_file {path} rejected: {e}"),
            })
        }
    }
}

/// Parse a single tool call from raw model output, tolerating surrounding prose
/// by extracting the first balanced JSON object.
pub fn parse_tool_call(text: &str) -> Result<Tool> {
    let json = extract_json_object(text)
        .ok_or_else(|| DcError::Eval("no JSON object found in model output".to_string()))?;
    serde_json::from_str::<Tool>(json).map_err(|e| DcError::Eval(format!("invalid tool call: {e}")))
}

/// Join `rel` onto `workspace`, rejecting absolute paths and `..` traversal
/// (spec 04 — sandboxed to the workspace root).
fn safe_join(workspace: &Path, rel: &str) -> Result<PathBuf> {
    let rp = Path::new(rel);
    if rp.is_absolute() {
        return Err(DcError::Eval(format!("absolute paths not allowed: {rel}")));
    }
    for c in rp.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(DcError::Eval(format!("path escapes workspace: {rel}"))),
        }
    }
    Ok(workspace.join(rp))
}

/// Find the first balanced `{...}` block, ignoring braces inside JSON strings.
fn extract_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        // Only ASCII delimiters matter; UTF-8 continuation bytes are >= 0x80 and
        // never collide with these, so byte scanning is safe.
        let ch = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else {
            match ch {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_tool_call() {
        let t = parse_tool_call(r#"{"tool":"read_file","path":"a.txt"}"#).unwrap();
        assert_eq!(
            t,
            Tool::ReadFile {
                path: "a.txt".into()
            }
        );
    }

    #[test]
    fn extracts_json_amid_prose_and_braces_in_strings() {
        let raw =
            "Sure, here:\n{\"tool\":\"write_file\",\"path\":\"x\",\"content\":\"a { b } c\"}\ndone";
        let t = parse_tool_call(raw).unwrap();
        assert_eq!(
            t,
            Tool::WriteFile {
                path: "x".into(),
                content: "a { b } c".into()
            }
        );
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_tool_call("no json here").is_err());
    }

    #[test]
    fn write_then_read_roundtrips_in_workspace() {
        let dir = std::env::temp_dir().join(format!("dc-core-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let w = Tool::WriteFile {
            path: "sub/f.txt".into(),
            content: "hello".into(),
        };
        assert!(matches!(execute(&w, &dir), ToolOutcome::Observation(_)));
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/f.txt")).unwrap(),
            "hello"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = std::env::temp_dir();
        match execute(
            &Tool::ReadFile {
                path: "../secret".into(),
            },
            &dir,
        ) {
            ToolOutcome::Observation(o) => assert!(o.contains("rejected"), "got: {o}"),
            _ => panic!("expected observation"),
        }
    }
}
