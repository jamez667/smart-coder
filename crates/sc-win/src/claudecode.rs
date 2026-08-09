//! Running **Claude Code** as a run kind (spec 22).
//!
//! Claude Code is a complete agent — it owns its own loop, its own tools and its own file
//! edits — so it is emphatically NOT a [`sc_model::ModelBackend`], which is a single-turn
//! completion seam where *this* harness owns the loop. Wiring it there would put two agent
//! loops in charge of one workspace, with smart-coder re-running tool calls that had already
//! run. Instead it is a `RunKind`: a subprocess whose event stream is translated into the
//! [`AgentEvent`] vocabulary every other run kind already emits, so the activity feed, the
//! chat panel and the phone mirror work unchanged.
//!
//! **The translation is pure and lives here**, separate from the spawning, because that is
//! what makes it testable: a function from one line of JSON to an event needs no child
//! process, so the whole format contract is proven on the host against recorded fixtures.

use sc_core::AgentEvent;

/// Whether the `claude` CLI is available to run.
///
/// Probing spawns a process, so callers cache the answer (see `App::claude_available`) rather
/// than asking per keystroke or per frame.
pub fn detect() -> bool {
    // `--version` is the cheapest thing it will answer, and a zero exit is the only signal
    // needed. A failure to spawn (not installed) and a non-zero exit (installed but broken)
    // are the same answer here: not usable.
    crate::proc::command("claude")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The arguments for a run over `task`.
///
/// Split out so the argument list is asserted in a test rather than buried in the spawn: the
/// output format is the contract this whole module is written against, and a silent change to
/// `--output-format` would turn every line into an unparseable one.
pub fn args(task: &str) -> Vec<String> {
    vec![
        "-p".to_string(),
        task.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // stream-json refuses to run without --verbose.
        "--verbose".to_string(),
    ]
}

/// What one line of the stream means to the UI.
///
/// Deliberately richer than `Option<AgentEvent>`: the terminal `result` line ends the run and
/// carries its own verdict, which the caller must turn into `UiEvent::Done` rather than another
/// agent event. Keeping that distinct here means the caller has no parsing left to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// An ordinary event to forward to the activity feed.
    Event(AgentEvent),
    /// The run ended. `ok` is Claude Code's own verdict; `summary` its closing text.
    Done { ok: bool, summary: String },
    /// Nothing the UI needs — a rate-limit notice, a thinking block, an unknown line.
    Ignored,
}

/// Translate one line of `--output-format stream-json` into UI events.
///
/// Returns a *list* because one `assistant` line can carry several content blocks — text plus
/// two tool calls is one line and three events.
///
/// **A line that cannot be parsed is [`Line::Ignored`], never an error.** The format belongs to
/// another project and will gain fields; a run must not die because one line was unexpected.
/// The caller counts them so the silence is reportable rather than invisible.
pub fn parse_line(line: &str) -> Vec<Line> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![Line::Ignored];
    };
    match v.get("type").and_then(|t| t.as_str()) {
        // The init banner names the session and the tools. Used only to open the feed with the
        // task; `prompt_budget` is 0 because Claude Code manages its own context (spec 05 is
        // about ours) and a plausible-looking number nothing measured would be a lie.
        Some("system") => vec![Line::Event(AgentEvent::RunStarted {
            task: "Claude Code".to_string(),
            prompt_budget: 0,
        })],
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
        // `rate_limit_event` and anything else new: not an error, just not ours.
        _ => vec![Line::Ignored],
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
fn assistant_block(b: &serde_json::Value) -> Option<AgentEvent> {
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            let raw = b.get("text").and_then(|t| t.as_str())?.to_string();
            // An empty text block carries nothing and would render as a blank row.
            if raw.trim().is_empty() {
                return None;
            }
            Some(AgentEvent::ModelTurn {
                step: 0,
                // Claude Code manages its own context; we did not assemble this prompt and
                // have no honest count for it.
                prompt_tokens: 0,
                raw,
            })
        }
        Some("tool_use") => {
            let tool = b.get("name").and_then(|n| n.as_str())?.to_string();
            Some(AgentEvent::ToolCall {
                arg: tool_arg(b.get("input")),
                tool,
            })
        }
        // `thinking` is the model's reasoning — often an empty stub with a signature. The
        // activity feed shows what the agent DID; reasoning would bury the tool calls.
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
    Some(Line::Event(AgentEvent::ToolResult {
        summary,
        full,
        is_error,
    }))
}

/// The one argument worth showing for a tool call.
///
/// Each tool names its principal argument differently — `Read` has `file_path`, `Bash` has
/// `command`, `Grep` has `pattern` — so this tries the known keys in order and falls back to
/// the whole input. A tool we've never heard of still renders something honest rather than an
/// empty line, which matters because the tool list grows without asking us.
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

    /// Fixture lines captured from a REAL `claude 2.1.126` run, not invented. The format is
    /// another project's contract, so a test written against a guess proves nothing.
    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"C:\\tmp","session_id":"783ccd96","tools":["Task","Bash"],"model":"claude-opus-4-7"}"#;
    const TEXT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-4-7","role":"assistant","content":[{"type":"text","text":"`hello.txt` contains a single line: `world`."}]}}"#;
    const TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Read","input":{"file_path":"C:\\tmp\\hello.txt"}}]}}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01","type":"tool_result","content":"1\tworld\n2\t"}]}}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":5019,"num_turns":2,"result":"`hello.txt` contains a single line: `world`.","total_cost_usd":0.089}"#;
    const RATE_LIMIT: &str =
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"},"uuid":"f00"}"#;

    #[test]
    fn a_text_block_becomes_a_model_turn() {
        assert_eq!(
            parse_line(TEXT),
            vec![Line::Event(AgentEvent::ModelTurn {
                step: 0,
                prompt_tokens: 0,
                raw: "`hello.txt` contains a single line: `world`.".to_string(),
            })]
        );
    }

    #[test]
    fn a_tool_use_block_names_the_tool_and_its_argument() {
        assert_eq!(
            parse_line(TOOL_USE),
            vec![Line::Event(AgentEvent::ToolCall {
                tool: "Read".to_string(),
                arg: r"C:\tmp\hello.txt".to_string(),
            })]
        );
    }

    #[test]
    fn a_tool_result_carries_its_first_line_as_the_summary() {
        let got = parse_line(TOOL_RESULT);
        assert_eq!(
            got,
            vec![Line::Event(AgentEvent::ToolResult {
                summary: "1\tworld".to_string(),
                full: "1\tworld\n2\t".to_string(),
                is_error: false,
            })]
        );
    }

    #[test]
    fn the_result_line_ends_the_run_with_its_own_verdict() {
        assert_eq!(
            parse_line(RESULT),
            vec![Line::Done {
                ok: true,
                summary: "`hello.txt` contains a single line: `world`.".to_string(),
            }]
        );
        // A failed run reports `ok: false` rather than being mistaken for success.
        let failed =
            r#"{"type":"result","subtype":"error","is_error":true,"result":"ran out of turns"}"#;
        assert_eq!(
            parse_line(failed),
            vec![Line::Done {
                ok: false,
                summary: "ran out of turns".to_string(),
            }]
        );
    }

    #[test]
    fn the_init_banner_opens_the_run() {
        assert_eq!(
            parse_line(INIT),
            vec![Line::Event(AgentEvent::RunStarted {
                task: "Claude Code".to_string(),
                // Claude Code manages its own context — 0 means "not applicable", and is
                // deliberately not a guess.
                prompt_budget: 0,
            })]
        );
    }

    /// **A line we cannot parse must never kill the run.**
    ///
    /// The format belongs to another project and will change. Every one of these is ignored
    /// rather than raised: garbage, a truncated write, an unknown event type, and an empty
    /// line. This is the test that earns the right to run a foreign format at all.
    #[test]
    fn an_unparseable_or_unknown_line_is_ignored_not_fatal() {
        assert_eq!(parse_line("not json at all"), vec![Line::Ignored]);
        assert_eq!(
            parse_line(r#"{"type":"assistant","messa"#),
            vec![Line::Ignored]
        );
        assert_eq!(parse_line(RATE_LIMIT), vec![Line::Ignored]);
        assert_eq!(
            parse_line(r#"{"type":"some_future_event","payload":1}"#),
            vec![Line::Ignored]
        );
        // Blank lines are not even worth counting as skipped.
        assert!(parse_line("").is_empty());
        assert!(parse_line("   ").is_empty());
    }

    /// A `thinking` block is dropped: the feed shows what the agent did, not what it mused.
    #[test]
    fn thinking_blocks_are_not_shown() {
        let thinking = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"Ev0B"}]}}"#;
        assert!(parse_line(thinking).is_empty());
    }

    /// One line can carry several blocks — text plus a tool call is one line, two events.
    #[test]
    fn one_line_can_produce_several_events() {
        let both = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"hmm"},
            {"type":"text","text":"Let me look."},
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}
        ]}}"#;
        let got = parse_line(both);
        assert_eq!(got.len(), 2, "the thinking block is dropped: {got:?}");
        assert!(
            matches!(&got[0], Line::Event(AgentEvent::ModelTurn { raw, .. }) if raw == "Let me look.")
        );
        assert!(
            matches!(&got[1], Line::Event(AgentEvent::ToolCall { tool, arg }) if tool == "Bash" && arg == "cargo test")
        );
    }

    /// An unknown tool still renders an argument rather than a blank row.
    #[test]
    fn an_unknown_tool_shape_still_shows_something() {
        let odd = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"FutureTool","input":{"whatsit":"42"}}]}}"#;
        let got = parse_line(odd);
        assert!(
            matches!(&got[0], Line::Event(AgentEvent::ToolCall { tool, arg }) if tool == "FutureTool" && arg.contains("whatsit")),
            "{got:?}"
        );
        // And a tool with no input at all is empty, not a panic.
        let bare = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bare"}]}}"#;
        assert!(
            matches!(&parse_line(bare)[0], Line::Event(AgentEvent::ToolCall { arg, .. }) if arg.is_empty())
        );
    }

    /// The argument list IS the contract this module parses against.
    #[test]
    fn the_arguments_pin_the_output_format() {
        let a = args("do the thing");
        assert!(a.contains(&"stream-json".to_string()), "{a:?}");
        assert!(
            a.contains(&"--verbose".to_string()),
            "stream-json refuses to run without it: {a:?}"
        );
        assert_eq!(a[1], "do the thing", "the task is passed verbatim");
    }
}
