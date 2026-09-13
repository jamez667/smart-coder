//! Translating Claude Code's `--output-format stream-json` into feed rows.
//!
//! **Pure, and that is the whole point.** A function from one line of JSON to a list of
//! events needs no child process, so the entire format contract is proven on the host
//! against recorded fixtures. That property came from `sc_win::claudecode`, which this is
//! ported from, and it is the reason the port is cheap: the hard part was already
//! separated from the process handling.
//!
//! # What changed in the port
//!
//! One thing: the events. The host version emits `sc_core::AgentEvent`, because it fed
//! the shared activity stream. A plugin has no `sc-core` — it speaks the plugin protocol
//! and nothing else — so it emits [`Event`], a four-variant enum that says exactly what
//! Claude Code produces and nothing more.
//!
//! That is a smaller type, not a lossy one. Only four `AgentEvent` variants were ever
//! constructed here, and two of their fields (`prompt_budget`, the token counts on
//! `ModelTurn`) were always zero with a comment explaining that Claude Code manages its
//! own context and a plausible-looking number nothing measured would be a lie. Dropping
//! fields that were always zero loses nothing.

/// What one line of the stream means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Something to put in the feed.
    Event(Event),
    /// The run ended. `ok` is Claude Code's own verdict; `summary` its closing text.
    Done { ok: bool, summary: String },
    /// A line we know about and deliberately don't show — a rate-limit notice, say.
    ///
    /// Distinct from [`Line::Unknown`] on purpose: the caller counts unknowns to report
    /// format drift, and folding the expected-but-uninteresting lines in with them means
    /// crying wolf on every run, which trains the user to ignore the one report that
    /// matters.
    Ignored,
    /// A line this build does not understand — malformed, or a type added since. Counted
    /// and reported, because a format change that silently halves the feed should be
    /// visible.
    Unknown,
}

/// One thing that happened during a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The run began.
    Started,
    /// The model said something. `raw` is markdown.
    Said(String),
    /// The model called a tool: its name, and the one argument worth showing.
    Called { tool: String, arg: String },
    /// A tool answered.
    Result {
        summary: String,
        full: String,
        is_error: bool,
    },
}

/// Translate one line, with tool paths made **workspace-relative** against `root`.
///
/// The relativising matters more than it looks: Claude Code reports absolute paths, and a
/// feed full of `C:\Users\...\project\src\main.rs` is a feed you cannot read at a glance.
pub fn parse_line_in(line: &str, root: &std::path::Path) -> Vec<Line> {
    let mut out = parse_line(line);
    for l in &mut out {
        if let Line::Event(Event::Called { arg, .. }) = l {
            if let Some(rel) = relativise(arg, root) {
                *arg = rel;
            }
        }
    }
    out
}

/// Strip `root` from an absolute path, yielding the workspace-relative form.
///
/// `None` when it isn't under the root (a file edited outside the project, or an argument
/// that was never a path at all — a `Bash` command, say), in which case the original is
/// kept.
fn relativise(arg: &str, root: &std::path::Path) -> Option<String> {
    let p = std::path::Path::new(arg);
    if !p.is_absolute() {
        return None;
    }
    p.strip_prefix(root)
        .ok()
        .and_then(|r| r.to_str())
        .map(|r| r.replace('\\', "/"))
}

/// Translate one line.
///
/// Returns a *list* because one `assistant` line can carry several content blocks — text
/// plus two tool calls is one line and three events.
///
/// **A line that cannot be parsed is [`Line::Unknown`], never an error.** The format
/// belongs to another project and will gain fields; a run must not die because one line
/// was unexpected. The caller counts them so the silence is reportable rather than
/// invisible.
pub fn parse_line(line: &str) -> Vec<Line> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![Line::Unknown];
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") => vec![Line::Event(Event::Started)],
        Some("assistant") => blocks(&v)
            .iter()
            .filter_map(assistant_block)
            .map(Line::Event)
            .collect(),
        Some("user") => blocks(&v).iter().filter_map(tool_result).collect(),
        Some("result") => {
            // `is_error` is Claude Code's own verdict on the run; `result` its closing text.
            let ok = !v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let summary = v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or(if ok { "finished" } else { "failed" })
                .to_string();
            vec![Line::Done { ok, summary }]
        }
        // Known, and deliberately not shown: a rate-limit notice is normal traffic.
        Some("rate_limit_event") => vec![Line::Ignored],
        // Anything else: not an error, but worth counting — it may be a type added since.
        _ => vec![Line::Unknown],
    }
}

/// The `message.content[]` blocks of a line, or empty when the shape isn't what we expect.
fn blocks(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
}

/// One block of an `assistant` message.
fn assistant_block(b: &serde_json::Value) -> Option<Event> {
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            let raw = b.get("text").and_then(|t| t.as_str())?.to_string();
            // An empty text block carries nothing and would render as a blank row.
            if raw.trim().is_empty() {
                return None;
            }
            Some(Event::Said(raw))
        }
        Some("tool_use") => {
            let tool = b.get("name").and_then(|n| n.as_str())?.to_string();
            Some(Event::Called {
                arg: tool_arg(b.get("input")),
                tool,
            })
        }
        // `thinking` is the model's reasoning — often an empty stub with a signature. The
        // feed shows what the agent DID; reasoning would bury the tool calls.
        _ => None,
    }
}

/// One block of a `user` message — the result of a tool the assistant called.
fn tool_result(b: &serde_json::Value) -> Option<Line> {
    if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
        return None;
    }
    // `content` is a string for most tools, but an array of blocks for some (an image, or
    // text split into parts). Flatten rather than dropping it.
    let full = match b.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let is_error = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    let summary = full.lines().next().unwrap_or_default().to_string();
    Some(Line::Event(Event::Result {
        summary,
        full,
        is_error,
    }))
}

/// The one argument worth showing for a tool call.
///
/// Each tool names its principal argument differently — `Read` has `file_path`, `Bash` has
/// `command`, `Grep` has `pattern` — so this tries the known keys in order and falls back
/// to the whole input. A tool we've never heard of still renders something honest rather
/// than an empty line, which matters because the tool list grows without asking us.
fn tool_arg(input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    const KEYS: [&str; 7] = [
        "file_path",
        "command",
        "pattern",
        "path",
        "url",
        "prompt",
        "description",
    ];
    for k in KEYS {
        if let Some(s) = input.get(k).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    // Compact JSON of the whole input: ugly, but never a blank row.
    let s = input.to_string();
    if s == "{}" {
        String::new()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_system_line_starts_the_run() {
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"init"}"#),
            vec![Line::Event(Event::Started)]
        );
    }

    #[test]
    fn an_assistant_text_block_is_something_said() {
        let got = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."}]}}"#,
        );
        assert_eq!(got, vec![Line::Event(Event::Said("Let me look.".into()))]);
    }

    /// One line, several blocks, several events — the reason this returns a `Vec`.
    #[test]
    fn one_line_can_carry_several_events() {
        let got = parse_line(
            r#"{"type":"assistant","message":{"content":[
                {"type":"text","text":"Checking."},
                {"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], Line::Event(Event::Said("Checking.".into())));
        assert_eq!(
            got[1],
            Line::Event(Event::Called {
                tool: "Bash".into(),
                arg: "cargo test".into()
            })
        );
    }

    /// An empty text block would render as a blank row, so it produces nothing.
    #[test]
    fn an_empty_text_block_produces_nothing() {
        let got = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"   "}]}}"#,
        );
        assert!(got.is_empty());
    }

    /// Reasoning is not shown: the feed is what the agent DID, and thinking would bury
    /// the tool calls.
    #[test]
    fn thinking_blocks_are_not_shown() {
        let got = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#,
        );
        assert!(got.is_empty());
    }

    #[test]
    fn a_tool_result_carries_its_first_line_as_the_summary() {
        let got = parse_line(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","content":"ok\nmore detail","is_error":false}]}}"#,
        );
        assert_eq!(
            got,
            vec![Line::Event(Event::Result {
                summary: "ok".into(),
                full: "ok\nmore detail".into(),
                is_error: false
            })]
        );
    }

    /// Some tools answer with an array of blocks rather than a string. Flatten rather
    /// than dropping the content.
    #[test]
    fn an_array_tool_result_is_flattened() {
        let got = parse_line(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","content":[{"type":"text","text":"one"},
                                                 {"type":"text","text":"two"}]}]}}"#,
        );
        let Line::Event(Event::Result { full, .. }) = &got[0] else {
            panic!("expected a result, got {got:?}");
        };
        assert_eq!(full, "one\ntwo");
    }

    #[test]
    fn a_result_line_ends_the_run() {
        assert_eq!(
            parse_line(r#"{"type":"result","is_error":false,"result":"done"}"#),
            vec![Line::Done {
                ok: true,
                summary: "done".into()
            }]
        );
        assert_eq!(
            parse_line(r#"{"type":"result","is_error":true,"result":"nope"}"#),
            vec![Line::Done {
                ok: false,
                summary: "nope".into()
            }]
        );
    }

    /// **The rule that keeps a run alive.** The format belongs to another project and
    /// will gain fields; an unexpected line must not kill the run.
    #[test]
    fn an_unknown_type_is_counted_not_fatal() {
        assert_eq!(parse_line(r#"{"type":"telepathy"}"#), vec![Line::Unknown]);
        assert_eq!(parse_line("not json at all"), vec![Line::Unknown]);
    }

    /// Known and deliberately silent, so the unknown counter does not cry wolf every run.
    #[test]
    fn a_rate_limit_notice_is_ignored_not_counted() {
        assert_eq!(
            parse_line(r#"{"type":"rate_limit_event"}"#),
            vec![Line::Ignored]
        );
    }

    #[test]
    fn a_blank_line_produces_nothing() {
        assert!(parse_line("").is_empty());
        assert!(parse_line("   ").is_empty());
    }

    /// Each tool names its principal argument differently; an unknown one still shows
    /// something honest rather than a blank row.
    #[test]
    fn the_shown_argument_is_found_per_tool() {
        let cases = [
            (r#"{"file_path":"/a/b.rs"}"#, "/a/b.rs"),
            (r#"{"command":"ls"}"#, "ls"),
            (r#"{"pattern":"foo"}"#, "foo"),
            (r#"{"url":"http://x"}"#, "http://x"),
        ];
        for (input, want) in cases {
            let v: serde_json::Value = serde_json::from_str(input).unwrap();
            assert_eq!(tool_arg(Some(&v)), want);
        }
        // Unknown shape: the whole input, never blank.
        let v: serde_json::Value = serde_json::from_str(r#"{"whatsit":1}"#).unwrap();
        assert!(tool_arg(Some(&v)).contains("whatsit"));
        // Genuinely empty: blank is honest.
        let v: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert_eq!(tool_arg(Some(&v)), "");
    }

    /// A feed full of absolute paths is one you cannot read at a glance.
    ///
    /// Platform-shaped on purpose: `is_absolute` is the first thing `relativise` checks,
    /// and `/proj/x` is NOT absolute on Windows — a Unix-shaped fixture would pass on CI
    /// and quietly assert nothing on the machine this is developed on.
    #[test]
    fn tool_paths_are_made_workspace_relative() {
        // `abs` is embedded in a JSON string, so a Windows path's separators have to be
        // escaped — `C:\proj` in JSON is `C:\\proj`. Getting that wrong yields a path
        // that parses as `C:projsrcmain.rs` and an assertion failure that looks like the
        // relativising is broken when it is the fixture.
        #[cfg(windows)]
        let (root, abs) = (std::path::Path::new(r"C:\proj"), r"C:\\proj\\src\\main.rs");
        #[cfg(not(windows))]
        let (root, abs) = (std::path::Path::new("/proj"), "/proj/src/main.rs");

        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[
                {{"type":"tool_use","name":"Read","input":{{"file_path":"{abs}"}}}}]}}}}"#
        );
        let got = parse_line_in(&line, root);
        assert_eq!(
            got[0],
            Line::Event(Event::Called {
                tool: "Read".into(),
                arg: "src/main.rs".into()
            }),
            "{got:?}"
        );
    }

    /// A path outside the workspace, and an argument that was never a path, are both kept
    /// as they came.
    #[test]
    fn a_non_path_argument_is_left_alone() {
        let root = std::path::Path::new("/proj");
        let got = parse_line_in(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            root,
        );
        assert_eq!(
            got[0],
            Line::Event(Event::Called {
                tool: "Bash".into(),
                arg: "cargo test".into()
            })
        );
    }
}
