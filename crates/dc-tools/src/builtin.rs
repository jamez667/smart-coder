//! The built-in v1 tool surface and its execution (spec 04 — built-in tool set).
//!
//! Deliberately tiny and narrow: a few sharply-scoped tools beat a broad,
//! ambiguous surface for a small model (spec 04). The surface spans read-only
//! navigation (`read_file`/`list_dir`/`search_code`/`find_symbol`), mutation
//! (`write_file`/`create_file`/anchored `edit_file`), and execution
//! (`run_command`/`run_verification`) — the latter two run processes, so the
//! agent loop executes them; this module is the pure-filesystem half.
//!
//! Every path is sandboxed to the workspace root; traversal outside it is
//! rejected. Execution never panics — tool errors become structured observations
//! the model can react to.

use std::path::{Component, Path, PathBuf};

use dc_proto::{DcError, Result};

use crate::spec::{
    ParamSpec, ParamType, Permission, SideEffect, ToolRegistry, ToolSpec, ValidatedCall,
};

/// The default registry: the v1 built-in tools, in a stable order.
pub fn default_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        ToolSpec {
            name: "read_file",
            description: "Read a UTF-8 text file's contents.",
            params: vec![ParamSpec::new(
                "path",
                ParamType::String,
                "file path relative to the project root",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "list_dir",
            description: "List the entries of a directory (non-recursive).",
            params: vec![ParamSpec::new(
                "path",
                ParamType::String,
                "directory path relative to the project root ('.' for root)",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "search_code",
            description: "Search files for a literal substring; returns file:line hits.",
            params: vec![ParamSpec::new(
                "query",
                ParamType::String,
                "the literal text to search for",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "find_symbol",
            description: "Locate where a function/type/class is defined; returns path:line.",
            params: vec![ParamSpec::new(
                "name",
                ParamType::String,
                "the symbol name to locate (exact)",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "write_file",
            description: "Create or overwrite a file with the given full contents.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("content", ParamType::String, "the full new file contents"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "create_file",
            description: "Create a NEW file with the given contents; fails if it already exists.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("content", ParamType::String, "the full file contents"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "append_file",
            description: "Append content to the END of a file (creating it if absent). Use this \
                          to build a large file in several turns: write the first part with \
                          write_file, then append the rest in chunks so no single reply is too long.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("content", ParamType::String, "text to append at the end of the file"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace an EXACT snippet in a file: old_str must occur exactly once.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "old_str",
                    ParamType::String,
                    "the exact text to replace (must appear exactly once)",
                ),
                ParamSpec::new("new_str", ParamType::String, "the replacement text"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "run_command",
            description: "Run a shell command in the workspace; returns exit code + output.",
            params: vec![ParamSpec::new(
                "command",
                ParamType::String,
                "the shell command line to run",
            )],
            side_effect: SideEffect::Destructive,
            permission: Permission::Confirm,
        },
        ToolSpec {
            name: "run_verification",
            description: "Run the project's configured test command; returns per-test results.",
            params: vec![],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "update_plan",
            description: "Replace your step plan with a new ordered list of short steps.",
            params: vec![ParamSpec::new(
                "steps",
                ParamType::String,
                "the new plan as a JSON array of short step strings",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "ask_user",
            description: "Escalate a genuine blocker for advice instead of guessing.",
            params: vec![ParamSpec::new(
                "question",
                ParamType::String,
                "the specific question or blocker",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "finish",
            description: "Declare the task complete.",
            params: vec![],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
    ])
}

/// A deliberately tiny registry for a focus-scoped worker (spec 04/08): just the
/// three tools it ever needs — `edit_file`, `run_verification`, `finish`. The
/// worker is already shown the file's current contents every turn, so it never
/// needs to read/search/list/plan/ask. Fewer choices = a dumb model that acts
/// instead of dithering between twelve options.
pub fn minimal_worker_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        ToolSpec {
            name: "edit_file",
            description: "Replace an exact snippet: old_str must match the shown file once.",
            params: vec![
                ParamSpec::new("path", ParamType::String, "the file to edit"),
                ParamSpec::new(
                    "old_str",
                    ParamType::String,
                    "exact text to replace, copied from the shown file",
                ),
                ParamSpec::new("new_str", ParamType::String, "the replacement text"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "run_verification",
            description: "Run the tests and see which pass or fail.",
            params: vec![],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "finish",
            description: "Stop — only once the tests pass.",
            params: vec![],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
    ])
}

/// The result of executing a validated tool call.
pub enum ToolOutcome {
    /// Text fed back to the model as the next observation.
    Observation(String),
    /// The model called `finish`.
    Finished,
}

/// Execute a *validated* call against `workspace`.
///
/// Because the call already passed [`ToolRegistry::validate`], the arguments are
/// known to be present and well-typed. Runtime failures (missing file, bad path)
/// still become observations, never panics.
pub fn execute(call: &ValidatedCall, workspace: &Path) -> ToolOutcome {
    match call.name.as_str() {
        "finish" => ToolOutcome::Finished,
        "read_file" => ToolOutcome::Observation(read_file(workspace, arg(call, "path"))),
        "list_dir" => ToolOutcome::Observation(list_dir(workspace, arg(call, "path"))),
        "search_code" => ToolOutcome::Observation(search_code(workspace, arg(call, "query"))),
        "write_file" => ToolOutcome::Observation(write_file(
            workspace,
            arg(call, "path"),
            arg(call, "content"),
        )),
        "create_file" => ToolOutcome::Observation(create_file(
            workspace,
            arg(call, "path"),
            arg(call, "content"),
        )),
        "append_file" => ToolOutcome::Observation(append_file(
            workspace,
            arg(call, "path"),
            arg(call, "content"),
        )),
        "edit_file" => ToolOutcome::Observation(edit_file(
            workspace,
            arg(call, "path"),
            arg(call, "old_str"),
            arg(call, "new_str"),
        )),
        // run_command / run_verification execute processes and need run config, so
        // the agent loop (dc-core) handles them; they never reach this fs executor.
        // The registry only dispatches names it knows; an unknown name here means
        // a tool was registered without a matching arm. Surface it loudly.
        other => ToolOutcome::Observation(format!("internal: no executor for tool {other:?}")),
    }
}

/// Pull a validated string arg. Safe to unwrap-with-default because validation
/// guaranteed required strings are present; optional/absent yields "".
fn arg<'a>(call: &'a ValidatedCall, name: &str) -> &'a str {
    call.str(name).unwrap_or_default()
}

fn read_file(workspace: &Path, path: &str) -> String {
    match safe_join(workspace, path) {
        Ok(p) => match std::fs::read_to_string(&p) {
            Ok(c) => format!("read_file {path}:\n{c}"),
            Err(e) => format!("read_file {path} error: {e}"),
        },
        Err(e) => format!("read_file {path} rejected: {e}"),
    }
}

fn list_dir(workspace: &Path, path: &str) -> String {
    let joined = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("list_dir {path} rejected: {e}"),
    };
    let mut entries = match std::fs::read_dir(&joined) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect::<Vec<_>>(),
        Err(e) => return format!("list_dir {path} error: {e}"),
    };
    entries.sort();
    if entries.is_empty() {
        format!("list_dir {path}: (empty)")
    } else {
        format!("list_dir {path}:\n{}", entries.join("\n"))
    }
}

/// A small literal-substring search over the workspace's text files. Skips the
/// usual noise dirs and anything that isn't valid UTF-8. Caps hits so the result
/// fits a small context window.
fn search_code(workspace: &Path, query: &str) -> String {
    const MAX_HITS: usize = 50;
    if query.is_empty() {
        return "search_code: empty query".to_string();
    }
    let mut hits = Vec::new();
    let mut walk = vec![workspace.to_path_buf()];
    while let Some(dir) = walk.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if matches!(name.as_str(), ".git" | "target" | "node_modules") {
                    continue;
                }
                walk.push(path);
            } else if let Ok(content) = std::fs::read_to_string(&path) {
                let rel = path
                    .strip_prefix(workspace)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                for (i, line) in content.lines().enumerate() {
                    if line.contains(query) {
                        hits.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                        if hits.len() >= MAX_HITS {
                            hits.sort();
                            return format!(
                                "search_code {query:?}: {MAX_HITS}+ hits (truncated):\n{}",
                                hits.join("\n")
                            );
                        }
                    }
                }
            }
        }
    }
    if hits.is_empty() {
        format!("search_code {query:?}: no matches")
    } else {
        hits.sort();
        format!(
            "search_code {query:?}: {} hit(s):\n{}",
            hits.len(),
            hits.join("\n")
        )
    }
}

fn write_file(workspace: &Path, path: &str, content: &str) -> String {
    match safe_join(workspace, path) {
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
    }
}

fn create_file(workspace: &Path, path: &str, content: &str) -> String {
    match safe_join(workspace, path) {
        Ok(p) => {
            if p.exists() {
                return format!(
                    "create_file {path} error: already exists (use edit_file or write_file)"
                );
            }
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&p, content) {
                Ok(()) => format!("create_file {path} ok ({} bytes)", content.len()),
                Err(e) => format!("create_file {path} error: {e}"),
            }
        }
        Err(e) => format!("create_file {path} rejected: {e}"),
    }
}

/// Append `content` to the end of a file, creating it (and any parent dirs) if it
/// doesn't exist. This is the escape hatch for building a file too large for a small
/// model to emit in one `write_file` reply: the model writes the head, then appends
/// the tail in bounded chunks so no single reply's JSON gets truncated mid-string.
fn append_file(workspace: &Path, path: &str, content: &str) -> String {
    use std::io::Write;
    match safe_join(workspace, path) {
        Ok(p) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
            {
                Ok(mut f) => match f.write_all(content.as_bytes()) {
                    Ok(()) => {
                        let total = std::fs::metadata(&p).map(|m| m.len()).unwrap_or_default();
                        format!(
                            "append_file {path} ok (+{} bytes, {total} total)",
                            content.len()
                        )
                    }
                    Err(e) => format!("append_file {path} error: {e}"),
                },
                Err(e) => format!("append_file {path} error: {e}"),
            }
        }
        Err(e) => format!("append_file {path} rejected: {e}"),
    }
}

/// Anchored edit: replace the single exact occurrence of `old_str` with `new_str`.
/// Turn literal escape sequences a model may have emitted as text (`\n`, `\t`,
/// `\r`, `\"`, `\\`) into the real characters — used as a fallback when a
/// small model writes `\\n` instead of a real newline inside `old_str`.
fn unescape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push('\n');
                    chars.next();
                }
                Some('t') => {
                    out.push('\t');
                    chars.next();
                }
                Some('r') => {
                    out.push('\r');
                    chars.next();
                }
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Render `content` with 1-based line numbers, so an edit error can point a small
/// model at exact anchors to copy.
fn number_lines(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, l)| format!("  {}: {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The "exactly once" rule is the small-model safety net (spec 04): an ambiguous
/// anchor (0 or >1 matches) is rejected with a precise count instead of guessing.
fn edit_file(workspace: &Path, path: &str, old_str: &str, new_str: &str) -> String {
    let p = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("edit_file {path} rejected: {e}"),
    };
    let raw = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return format!("edit_file {path} error: {e}"),
    };
    if old_str.is_empty() {
        return format!("edit_file {path} error: old_str must not be empty");
    }
    // Normalize line endings to LF for matching/editing. A small model's anchor is
    // LF-only, but a file checked out on Windows is CRLF — without this the anchor
    // never matches (the \r breaks it). We edit in LF space and write LF; that's
    // correct for source files and what the model expects.
    let content = raw.replace("\r\n", "\n");
    // Small models also emit a literal backslash-n (`\\n`) instead of a real
    // newline inside a multi-line old_str. Resolve the anchor to whichever form the
    // (normalized) file actually contains, un-escaping new_str to match.
    let (old_owned, new_owned) = if content.contains(old_str) {
        (old_str.to_string(), new_str.to_string())
    } else {
        let unescaped = unescape_literal(old_str);
        if unescaped != old_str && content.contains(&unescaped) {
            (unescaped, unescape_literal(new_str))
        } else {
            (old_str.to_string(), new_str.to_string())
        }
    };
    edit_file_with(&p, path, &content, &old_owned, &new_owned)
}

/// Apply an `old_str`→`new_str` replacement to already-read `content` at `p`,
/// enforcing the exactly-once rule (with whole-line disambiguation and
/// self-correcting errors for small models).
fn edit_file_with(p: &Path, path: &str, content: &str, old_str: &str, new_str: &str) -> String {
    let count = content.matches(old_str).count();
    if count == 0 {
        // The anchor isn't in the file. The usual cause for a small model is that
        // the edit already landed (or it's working from a stale view), so it keeps
        // re-proposing a change that's no longer applicable. Show the CURRENT file
        // with line numbers so it re-anchors on what's actually there now.
        let numbered = number_lines(content);
        return format!(
            "edit_file {path} error: old_str {old_str:?} not found (0 matches). The file may \
             already have that change. Here is the CURRENT content — pick your next anchor \
             from these exact lines:\n{numbered}"
        );
    }
    if count > 1 {
        // Whole-line disambiguation (spec 04 — do the work the small model can't).
        // A bare anchor like "return n" substring-matches both `    return n` and
        // `    return n % 2 == 0`. But as a *whole trimmed line* it matches exactly
        // one (`    return n`), which is unambiguously what the model meant. When
        // `old_str.trim()` equals exactly one line's trimmed text, edit that line
        // in place, preserving its indentation.
        let lines: Vec<&str> = content.lines().collect();
        let needle = old_str.trim();
        let line_hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == needle)
            .map(|(i, _)| i)
            .collect();
        if line_hits.len() == 1 {
            let i = line_hits[0];
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            let trailing_newline = content.ends_with('\n');
            let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            out[i] = format!("{indent}{}", new_str.trim());
            let mut joined = out.join("\n");
            if trailing_newline {
                joined.push('\n');
            }
            return match std::fs::write(p, &joined) {
                Ok(()) => format!(
                    "edit_file {path} ok (1 replacement, matched whole line {})",
                    i + 1
                ),
                Err(e) => format!("edit_file {path} error: {e}"),
            };
        }

        // Couldn't disambiguate automatically — show each matching line with its
        // number so the model can copy a longer, unique anchor.
        let mut shown = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.contains(old_str) {
                shown.push(format!("  line {}: {}", i + 1, line));
            }
        }
        return format!(
            "edit_file {path} error: old_str {old_str:?} is ambiguous ({count} matches). \
             Pick a UNIQUE anchor — copy a whole distinct line (or two) from below verbatim:\n{}",
            shown.join("\n")
        );
    }
    let updated = content.replacen(old_str, new_str, 1);
    match std::fs::write(p, &updated) {
        Ok(()) => format!("edit_file {path} ok (1 replacement)"),
        Err(e) => format!("edit_file {path} error: {e}"),
    }
}

/// List the **source** files actually on disk under `workspace` (workspace-relative,
/// `/`-separated, sorted), excluding test files and tooling caches/deps. This is
/// filesystem ground truth — what the run has *really* built so far, independent of the
/// model's own action history — so the agent loop can show the model a progress ledger and
/// stop it re-creating files that already exist (spec 03/05). Mirrors
/// `dc_win::config::source_files`; kept in sync deliberately.
pub fn source_files(workspace: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip hidden/dot dirs (.dumb-coder, .pytest_cache, .git), caches, deps.
            if name.starts_with('.') || name == "__pycache__" || name == "node_modules" {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !is_test_file(&rel) {
                        out.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Whether a workspace-relative path looks like a test file (so it's excluded from the
/// source-file ledger — the tests are frozen, not the run's output).
fn is_test_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("test_")
        || lower.contains(".test.")
        || lower.contains("_test.")
        || lower.contains(".spec.")
}

/// Join `rel` onto `workspace`, rejecting absolute paths and `..` traversal
/// (spec 04 — sandboxed to the workspace root).
pub fn safe_join(workspace: &Path, rel: &str) -> Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-tools-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call(v: serde_json::Value) -> ValidatedCall {
        default_registry().validate(&v).unwrap()
    }

    #[test]
    fn default_registry_has_the_v1_tools() {
        let names: Vec<_> = default_registry().specs().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "read_file",
                "list_dir",
                "search_code",
                "find_symbol",
                "write_file",
                "create_file",
                "append_file",
                "edit_file",
                "run_command",
                "run_verification",
                "update_plan",
                "ask_user",
                "finish"
            ]
        );
    }

    #[test]
    fn write_then_read_roundtrips() {
        let ws = temp_dir("rw");
        let w = call(json!({"tool":"write_file","path":"sub/f.txt","content":"hello"}));
        assert!(matches!(execute(&w, &ws), ToolOutcome::Observation(_)));

        let r = call(json!({"tool":"read_file","path":"sub/f.txt"}));
        match execute(&r, &ws) {
            ToolOutcome::Observation(o) => assert!(o.contains("hello"), "got: {o}"),
            _ => panic!("expected observation"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn obs(out: ToolOutcome) -> String {
        match out {
            ToolOutcome::Observation(o) => o,
            _ => panic!("expected observation"),
        }
    }

    #[test]
    fn create_file_writes_new_but_refuses_existing() {
        let ws = temp_dir("create");
        let c = call(json!({"tool":"create_file","path":"n.txt","content":"hi"}));
        assert!(obs(execute(&c, &ws)).contains("ok"));
        assert_eq!(std::fs::read_to_string(ws.join("n.txt")).unwrap(), "hi");
        // Second create on the same path is refused, not silently overwritten.
        let again = obs(execute(&c, &ws));
        assert!(again.contains("already exists"), "got: {again}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn append_file_creates_then_appends() {
        let ws = temp_dir("append");
        // First append creates the file.
        let a1 = call(json!({"tool":"append_file","path":"big.css","content":"a {}\n"}));
        assert!(obs(execute(&a1, &ws)).contains("ok"));
        // Second append adds to the end, not overwrites.
        let a2 = call(json!({"tool":"append_file","path":"big.css","content":"b {}\n"}));
        let o = obs(execute(&a2, &ws));
        assert!(o.contains("ok") && o.contains("total"), "got: {o}");
        assert_eq!(
            std::fs::read_to_string(ws.join("big.css")).unwrap(),
            "a {}\nb {}\n",
            "append concatenates in order"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn edit_file_replaces_a_unique_anchor() {
        let ws = temp_dir("edit-ok");
        std::fs::write(ws.join("a.rs"), "fn f() { return 1; }\n").unwrap();
        let e = call(json!({
            "tool":"edit_file","path":"a.rs",
            "old_str":"return 1;","new_str":"return 2;"
        }));
        assert!(obs(execute(&e, &ws)).contains("1 replacement"));
        assert_eq!(
            std::fs::read_to_string(ws.join("a.rs")).unwrap(),
            "fn f() { return 2; }\n"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn edit_file_rejects_missing_anchor() {
        let ws = temp_dir("edit-miss");
        std::fs::write(ws.join("a.rs"), "fn f() {}\n").unwrap();
        let e = call(json!({
            "tool":"edit_file","path":"a.rs","old_str":"nope","new_str":"x"
        }));
        let o = obs(execute(&e, &ws));
        assert!(o.contains("0 matches"), "got: {o}");
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(ws.join("a.rs")).unwrap(),
            "fn f() {}\n"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn edit_file_rejects_ambiguous_anchor() {
        let ws = temp_dir("edit-amb");
        std::fs::write(ws.join("a.rs"), "x\nx\n").unwrap();
        let e = call(json!({"tool":"edit_file","path":"a.rs","old_str":"x","new_str":"y"}));
        let o = obs(execute(&e, &ws));
        assert!(
            o.contains("ambiguous") && o.contains("2 matches"),
            "got: {o}"
        );
        // The error lists each matching line so the model can pick a unique anchor.
        assert!(o.contains("line 1:") && o.contains("line 2:"), "got: {o}");
        // Untouched — never edits on ambiguity.
        assert_eq!(std::fs::read_to_string(ws.join("a.rs")).unwrap(), "x\nx\n");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn edit_file_tolerates_literal_backslash_n_in_old_str() {
        // A small model writes "\\n" (literal backslash-n) instead of a real
        // newline in a multi-line old_str. The harness un-escapes and matches.
        let ws = temp_dir("edit-escn");
        std::fs::write(ws.join("m.py"), "def is_even(n):\n    return False\n").unwrap();
        let e = call(json!({
            "tool":"edit_file","path":"m.py",
            "old_str":"def is_even(n):\\n    return False",
            "new_str":"def is_even(n):\\n    return n % 2 == 0"
        }));
        let o = obs(execute(&e, &ws));
        assert!(o.contains("1 replacement"), "got: {o}");
        assert_eq!(
            std::fs::read_to_string(ws.join("m.py")).unwrap(),
            "def is_even(n):\n    return n % 2 == 0\n",
            "real newlines applied, not literal backslash-n"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn edit_file_disambiguates_by_whole_line() {
        // "return n" substring-matches two lines, but as a whole trimmed line it
        // matches exactly one — the harness edits that line in place, preserving
        // indentation. (This is the mathlib `double` case from the live swarm.)
        let ws = temp_dir("edit-wholeline");
        std::fs::write(
            ws.join("m.py"),
            "def is_even(n):\n    return n % 2 == 0\n\n\ndef double(n):\n    return n\n",
        )
        .unwrap();
        let e = call(json!({
            "tool":"edit_file","path":"m.py","old_str":"return n","new_str":"return n * 2"
        }));
        let o = obs(execute(&e, &ws));
        assert!(o.contains("whole line"), "got: {o}");
        assert_eq!(
            std::fs::read_to_string(ws.join("m.py")).unwrap(),
            "def is_even(n):\n    return n % 2 == 0\n\n\ndef double(n):\n    return n * 2\n",
            "only the double body line changed, indentation preserved"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn list_dir_sorts_and_marks_directories() {
        let ws = temp_dir("ls");
        std::fs::create_dir(ws.join("zdir")).unwrap();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        let o = match execute(&call(json!({"tool":"list_dir","path":"."})), &ws) {
            ToolOutcome::Observation(o) => o,
            _ => panic!(),
        };
        let body = o.split_once('\n').unwrap().1;
        assert_eq!(body, "a.txt\nzdir/");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn search_code_finds_literal_hits_with_line_numbers() {
        let ws = temp_dir("search");
        std::fs::write(ws.join("a.rs"), "fn one() {}\nfn target() {}\n").unwrap();
        std::fs::write(ws.join("b.rs"), "nothing here\n").unwrap();
        let o = match execute(&call(json!({"tool":"search_code","query":"target"})), &ws) {
            ToolOutcome::Observation(o) => o,
            _ => panic!(),
        };
        assert!(o.contains("a.rs:2"), "got: {o}");
        assert!(!o.contains("b.rs"), "got: {o}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn search_code_reports_no_matches() {
        let ws = temp_dir("search-none");
        std::fs::write(ws.join("a.rs"), "x\n").unwrap();
        let o = match execute(&call(json!({"tool":"search_code","query":"zzz"})), &ws) {
            ToolOutcome::Observation(o) => o,
            _ => panic!(),
        };
        assert!(o.contains("no matches"), "got: {o}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn finish_is_finished() {
        let ws = temp_dir("fin");
        assert!(matches!(
            execute(&call(json!({"tool":"finish"})), &ws),
            ToolOutcome::Finished
        ));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn source_files_lists_real_files_excluding_tests_and_caches() {
        let ws = temp_dir("srcfiles");
        std::fs::create_dir_all(ws.join("templates")).unwrap();
        std::fs::create_dir_all(ws.join("static")).unwrap();
        std::fs::create_dir_all(ws.join("__pycache__")).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join("app.py"), "x").unwrap();
        std::fs::write(ws.join("templates/board.html"), "x").unwrap();
        std::fs::write(ws.join("static/board.js"), "x").unwrap();
        std::fs::write(ws.join("test_app.py"), "x").unwrap(); // frozen test → excluded
        std::fs::write(ws.join("__pycache__/app.pyc"), "x").unwrap(); // cache → excluded
        std::fs::write(ws.join(".git/config"), "x").unwrap(); // dot-dir → excluded

        let files = source_files(&ws);
        assert_eq!(
            files,
            vec![
                "app.py".to_string(),
                "static/board.js".to_string(),
                "templates/board.html".to_string(),
            ],
            "only real sources, sorted, '/'-sep; tests/cache/dot-dirs excluded"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn source_files_is_empty_for_a_fresh_dir() {
        let ws = temp_dir("srcfiles-empty");
        assert!(source_files(&ws).is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn rejects_path_traversal() {
        let ws = temp_dir("trav");
        match execute(&call(json!({"tool":"read_file","path":"../secret"})), &ws) {
            ToolOutcome::Observation(o) => assert!(o.contains("rejected"), "got: {o}"),
            _ => panic!(),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }
}
