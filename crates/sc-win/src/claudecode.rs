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

/// Which model a run uses. `Default` passes no `--model`, deferring to the CLI's own choice —
/// which is the honest default, since that choice is Claude Code's to make and it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    #[default]
    Default,
    Opus,
    Sonnet,
    Haiku,
}

impl Model {
    /// The `--model` alias. `None` for [`Model::Default`] — the flag is omitted entirely.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Model::Default => None,
            Model::Opus => Some("opus"),
            Model::Sonnet => Some("sonnet"),
            Model::Haiku => Some("haiku"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Model::Default => "Default",
            Model::Opus => "Opus",
            Model::Sonnet => "Sonnet",
            Model::Haiku => "Haiku",
        }
    }
    /// The cycle order for the menu's selector.
    pub const ALL: [Model; 4] = [Model::Default, Model::Opus, Model::Sonnet, Model::Haiku];

    /// Parse a persisted alias. Unknown ⇒ [`Model::Default`], never a guess — a config naming a
    /// model this build doesn't know should fall back to the CLI's own choice rather than
    /// pinning something arbitrary.
    pub fn from_slug(s: &str) -> Self {
        match s.trim() {
            "opus" => Model::Opus,
            "sonnet" => Model::Sonnet,
            "haiku" => Model::Haiku,
            _ => Model::Default,
        }
    }
}

/// How much Claude Code asks before acting.
///
/// **`BypassPermissions` is deliberately not offered.** It lets an agent take every action
/// without asking, in the user's real project, with no gate anywhere in this app — spec 00's
/// "no unattended *approval*" non-goal is about exactly that judgement, and a one-click path to
/// it in a side menu is not a considered decision. Someone who genuinely wants it can run the
/// CLI directly, where the choice is at least explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Permission {
    /// Claude Code asks as it normally would.
    #[default]
    Default,
    /// File edits are auto-accepted; other tools still ask.
    AcceptEdits,
    /// Plan only — it works out what it would do without doing it.
    Plan,
}

impl Permission {
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Permission::Default => None,
            Permission::AcceptEdits => Some("acceptEdits"),
            Permission::Plan => Some("plan"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Permission::Default => "Ask as usual",
            Permission::AcceptEdits => "Auto-accept edits",
            Permission::Plan => "Plan only",
        }
    }
    pub const ALL: [Permission; 3] = [
        Permission::Default,
        Permission::AcceptEdits,
        Permission::Plan,
    ];

    /// Parse a persisted mode. **Unknown ⇒ [`Permission::Default`]**, which is the mode that
    /// asks the most. That direction is deliberate: a config carrying `bypassPermissions` —
    /// hand-edited, or written by some future build — must fall back to asking rather than to
    /// the permissive thing it names.
    pub fn from_slug(s: &str) -> Self {
        match s.trim() {
            "acceptEdits" => Permission::AcceptEdits,
            "plan" => Permission::Plan,
            _ => Permission::Default,
        }
    }
}

/// Everything the panel's ⚙ menu can set for a run (spec 22).
///
/// One struct so [`args`] has a single input and the whole flag surface is asserted in one
/// place. Every field's default is "pass no flag", so a fresh install behaves exactly as the
/// CLI would on its own — the options add to Claude Code's behaviour, they never silently
/// replace it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub model: Model,
    pub permission: Permission,
    /// Carry the previous run's context (`--continue`) instead of starting cold.
    pub continue_session: bool,
    /// Resume one SPECIFIC past conversation (`--resume <id>`), chosen from the
    /// picker. Takes precedence over [`Self::continue_session`], which only ever
    /// means "the most recent".
    pub resume_session: Option<String>,
    /// Extra directories the run may touch, beyond the workspace (`--add-dir`).
    pub add_dirs: Vec<String>,
    /// Restrict the run to these tools (`--allowedTools`). Empty ⇒ no restriction.
    pub allowed_tools: Vec<String>,
    /// Forbid these tools (`--disallowedTools`). Empty ⇒ nothing forbidden.
    pub disallowed_tools: Vec<String>,
}

/// Split a tool list on whitespace, but **keep bracketed patterns whole**.
///
/// The CLI's own examples include `Bash(git *)` — a single tool spec containing a space. A
/// naive `split_whitespace` turns that into `Bash(git` and `*)`, neither of which names a tool,
/// so a restriction the user carefully typed silently stops restricting anything.
pub fn split_tools(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The arguments for a run over `task`, with the panel's options applied.
///
/// Split out so the argument list is asserted in a test rather than buried in the spawn: the
/// output format is the contract this whole module is written against, and a silent change to
/// `--output-format` would turn every line into an unparseable one.
pub fn args(task: &str, opts: &Options) -> Vec<String> {
    let mut v = vec![
        "-p".to_string(),
        task.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // stream-json refuses to run without --verbose.
        "--verbose".to_string(),
    ];
    if let Some(m) = opts.model.flag() {
        v.push("--model".to_string());
        v.push(m.to_string());
    }
    if let Some(p) = opts.permission.flag() {
        v.push("--permission-mode".to_string());
        v.push(p.to_string());
    }
    // A specific session wins over "the most recent": the picker is an explicit
    // choice and `--continue` is a default, and passing both would be two flags
    // asking for different conversations.
    if let Some(id) = &opts.resume_session {
        v.push("--resume".to_string());
        v.push(id.clone());
    } else if opts.continue_session {
        v.push("--continue".to_string());
    }
    for d in &opts.add_dirs {
        v.push("--add-dir".to_string());
        v.push(d.clone());
    }
    // Space-separated in ONE argument, which is the shape the CLI documents. Passing each tool
    // as its own argv entry works for bare names but breaks a pattern like `Bash(git *)`, whose
    // space would then split it into two tools that mean nothing.
    if !opts.allowed_tools.is_empty() {
        v.push("--allowedTools".to_string());
        v.push(opts.allowed_tools.join(" "));
    }
    if !opts.disallowed_tools.is_empty() {
        v.push("--disallowedTools".to_string());
        v.push(opts.disallowed_tools.join(" "));
    }
    v
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
    /// A line we know about and deliberately don't show — a rate-limit notice, say.
    ///
    /// Distinct from [`Line::Unknown`] on purpose: the caller counts unknowns to report format
    /// drift, and folding the expected-but-uninteresting lines in with them means crying wolf
    /// on every single run, which trains the user to ignore the one report that matters.
    Ignored,
    /// A line this build does not understand — malformed, or a type added since. Counted and
    /// reported, because a format change that silently halves the feed should be visible.
    Unknown,
}

/// Translate one line of `--output-format stream-json` into UI events, with tool paths made
/// **workspace-relative** against `root`.
///
/// The relativising matters more than it looks: Claude Code reports absolute paths, and every
/// consumer downstream — the edited-files banner, the code pane's follow-the-agent pinning, the
/// file tree highlight — keys on workspace-relative ones. Doing it here means none of them
/// needs to know which run kind produced the event.
pub fn parse_line_in(line: &str, root: &std::path::Path) -> Vec<Line> {
    let mut out = parse_line(line);
    for l in &mut out {
        if let Line::Event(AgentEvent::ToolCall { arg, .. }) = l {
            if let Some(rel) = relativise(arg, root) {
                *arg = rel;
            }
        }
    }
    out
}

/// Strip `root` from an absolute path, yielding the workspace-relative form.
///
/// `None` when it isn't under the root (a file edited outside the project, or an argument that
/// was never a path at all — a `Bash` command, say), in which case the original is kept.
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
        return vec![Line::Unknown];
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
        // UNKNOWN — counted and reported, because it may be format drift.
        assert_eq!(parse_line("not json at all"), vec![Line::Unknown]);
        assert_eq!(
            parse_line(r#"{"type":"assistant","messa"#),
            vec![Line::Unknown]
        );
        assert_eq!(
            parse_line(r#"{"type":"some_future_event","payload":1}"#),
            vec![Line::Unknown]
        );
        // Blank lines are not even worth counting as skipped.
        assert!(parse_line("").is_empty());
        assert!(parse_line("   ").is_empty());
    }

    /// **A rate-limit notice is normal traffic, not format drift.**
    ///
    /// It appears on essentially every run. Counting it as "unrecognised" put a warning at the
    /// end of every single run — and a warning that always fires is one nobody reads, which
    /// would cost us the one report that actually matters.
    #[test]
    fn a_known_but_uninteresting_line_is_not_counted_as_drift() {
        assert_eq!(parse_line(RATE_LIMIT), vec![Line::Ignored]);
        assert_ne!(
            parse_line(RATE_LIMIT),
            vec![Line::Unknown],
            "must not be reported as unrecognised"
        );
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

    /// Tool paths come back workspace-relative, because everything downstream keys on that.
    ///
    /// Claude Code reports ABSOLUTE paths. Left as-is they would defeat the edited-files
    /// banner, the code pane's follow-the-agent pinning and the file-tree highlight — three
    /// silent failures, since each would simply match nothing.
    #[test]
    fn tool_paths_are_made_workspace_relative() {
        let root = std::path::Path::new(r"C:\proj");
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"C:\\proj\\src\\main.rs"}}]}}"#;
        let got = parse_line_in(line, root);
        assert!(
            matches!(&got[0], Line::Event(AgentEvent::ToolCall { arg, .. }) if arg == "src/main.rs"),
            "{got:?}"
        );

        // A path OUTSIDE the workspace is kept whole rather than mangled into a wrong
        // relative path — better an absolute path in the feed than a plausible lie.
        let outside = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"C:\\elsewhere\\notes.md"}}]}}"#;
        assert!(
            matches!(&parse_line_in(outside, root)[0], Line::Event(AgentEvent::ToolCall { arg, .. }) if arg == r"C:\elsewhere\notes.md")
        );

        // A non-path argument (a shell command) is untouched.
        let bash = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        assert!(
            matches!(&parse_line_in(bash, root)[0], Line::Event(AgentEvent::ToolCall { arg, .. }) if arg == "cargo test")
        );
    }

    /// The argument list IS the contract this module parses against.
    #[test]
    fn the_arguments_pin_the_output_format() {
        let a = args("do the thing", &Options::default());
        assert!(a.contains(&"stream-json".to_string()), "{a:?}");
        assert!(
            a.contains(&"--verbose".to_string()),
            "stream-json refuses to run without it: {a:?}"
        );
        assert_eq!(a[1], "do the thing", "the task is passed verbatim");
    }

    /// **Defaults add nothing.** A fresh install must behave exactly as the bare CLI would —
    /// the panel's options extend Claude Code's behaviour, they never silently replace it.
    #[test]
    fn default_options_pass_no_extra_flags() {
        let a = args("t", &Options::default());
        for flag in [
            "--model",
            "--permission-mode",
            "--continue",
            "--add-dir",
            "--allowedTools",
            "--disallowedTools",
        ] {
            assert!(!a.contains(&flag.to_string()), "{flag} leaked: {a:?}");
        }
    }

    /// **A picked session beats "the most recent".**
    ///
    /// `--continue` and `--resume <id>` both reopen a conversation, but they name
    /// different ones, and passing both asks the CLI for two things at once. The
    /// picker is an explicit choice; `--continue` is a default, so the explicit one
    /// wins and the default is dropped.
    #[test]
    fn a_resumed_session_replaces_continue() {
        let opts = Options {
            continue_session: true,
            resume_session: Some("abc-123".to_string()),
            ..Options::default()
        };
        let a = args("t", &opts);
        assert!(a.contains(&"--resume".to_string()), "{a:?}");
        assert!(a.contains(&"abc-123".to_string()), "{a:?}");
        assert!(
            !a.contains(&"--continue".to_string()),
            "both flags would name different conversations: {a:?}"
        );
    }

    /// With no session picked, `--continue` still works as before.
    #[test]
    fn continue_alone_is_unchanged() {
        let opts = Options {
            continue_session: true,
            ..Options::default()
        };
        let a = args("t", &opts);
        assert!(a.contains(&"--continue".to_string()), "{a:?}");
        assert!(!a.contains(&"--resume".to_string()), "{a:?}");
    }

    /// Each option becomes its flag.
    #[test]
    fn options_become_flags() {
        let a = args(
            "t",
            &Options {
                model: Model::Sonnet,
                permission: Permission::AcceptEdits,
                continue_session: true,
                // No session picked, so --continue is what this asserts.
                resume_session: None,
                add_dirs: vec![r"C:\other".to_string()],
                allowed_tools: vec!["Edit".to_string(), "Bash(git *)".to_string()],
                disallowed_tools: vec!["WebFetch".to_string()],
            },
        );
        let pair = |k: &str| {
            a.iter()
                .position(|x| x == k)
                .and_then(|i| a.get(i + 1))
                .cloned()
        };
        assert_eq!(pair("--model").as_deref(), Some("sonnet"));
        assert_eq!(pair("--permission-mode").as_deref(), Some("acceptEdits"));
        assert!(a.contains(&"--continue".to_string()));
        assert_eq!(pair("--add-dir").as_deref(), Some(r"C:\other"));
        // ONE argument, space-separated: a per-tool argv entry would split `Bash(git *)` at its
        // space into two tools that mean nothing.
        assert_eq!(pair("--allowedTools").as_deref(), Some("Edit Bash(git *)"));
        assert_eq!(pair("--disallowedTools").as_deref(), Some("WebFetch"));
    }

    /// A bracketed tool pattern survives splitting.
    ///
    /// `Bash(git *)` contains a space, so a naive whitespace split yields `Bash(git` and `*)` —
    /// neither of which names a tool, and the restriction then silently permits everything.
    #[test]
    fn a_bracketed_tool_pattern_is_not_split_at_its_space() {
        assert_eq!(
            split_tools("Edit Bash(git *) Read"),
            vec!["Edit", "Bash(git *)", "Read"]
        );
        assert_eq!(split_tools(""), Vec::<String>::new());
        assert_eq!(split_tools("   "), Vec::<String>::new());
        // Nested and unbalanced brackets do not panic or swallow the rest of the list.
        assert_eq!(split_tools("A(b (c)) D"), vec!["A(b (c))", "D"]);
        assert_eq!(split_tools("A(b D"), vec!["A(b D"]);
    }

    /// A persisted value this build doesn't recognise falls back to the SAFE end.
    #[test]
    fn an_unknown_persisted_option_falls_back_safely() {
        assert_eq!(Model::from_slug("gpt-9"), Model::Default);
        assert_eq!(Permission::from_slug("nonsense"), Permission::Default);
        // The one that matters: a config naming the permissive mode must not enable it.
        assert_eq!(
            Permission::from_slug("bypassPermissions"),
            Permission::Default,
            "a config naming the bypass mode must fall back to asking"
        );
    }

    /// **`bypassPermissions` is not reachable from the UI.**
    ///
    /// It lets an agent take every action without asking, in a real project, with no gate
    /// anywhere in this app. Spec 00's "no unattended *approval*" non-goal is about exactly
    /// that judgement, and a one-click path to it in a side menu is not a considered decision.
    #[test]
    fn the_bypass_permission_mode_is_not_offered() {
        for p in Permission::ALL {
            assert_ne!(p.flag(), Some("bypassPermissions"), "{p:?}");
        }
        // And no combination of the offered options can produce the flag value.
        for p in Permission::ALL {
            let a = args(
                "t",
                &Options {
                    permission: p,
                    ..Options::default()
                },
            );
            assert!(
                !a.iter().any(|x| x == "bypassPermissions"),
                "{p:?} produced it: {a:?}"
            );
        }
    }
}
