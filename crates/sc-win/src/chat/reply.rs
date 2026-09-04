//! Reading a raw assistant reply back: prose vs proposed files, command blocks, and
//! stripping the control tokens and `<think>` blocks that must never reach a chat bubble.

use super::ProposedFile;

/// Split a raw assistant reply into (prose, proposed-files). The prose is everything outside
/// ```file:NAME fenced blocks; each such block becomes a [`ProposedFile`]. A plain ``` block
/// (no `file:` info string) is left inline in the prose (it's an example, not a plan file).
pub fn parse_reply(reply: &str) -> (String, Vec<ProposedFile>) {
    // Defensively strip a reasoning model's <think>…</think> block (Qwen3 et al.) if one
    // leaked through despite the /no_think directive — it must never show in the chat.
    let reply = strip_think(reply);
    let reply = reply.as_str();
    let mut prose = String::new();
    let mut files = Vec::new();
    let mut lines = reply.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(name) = fence_file_name(line) {
            // Collect until the closing ```.
            let mut body = String::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            // Trim one trailing newline for a clean file body.
            if body.ends_with('\n') {
                body.pop();
            }
            files.push(ProposedFile {
                name: name.to_string(),
                content: body,
                applied: false,
            });
        } else if is_command_fence(line) {
            // A ```command block: swallow it (its content is surfaced separately as a
            // proposed command via `extract_command`) so it never lands in the chat prose.
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
            }
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    (prose.trim().to_string(), files)
}

/// The command line from a ```command block in `reply`, if present (the first one). Returns
/// the trimmed single command, or `None` if there's no command block. The app offers this as a
/// one-click Run in the integrated terminal (see the `Command` intent).
pub fn extract_command(reply: &str) -> Option<String> {
    let reply = strip_think(reply);
    let mut lines = reply.lines();
    while let Some(line) = lines.next() {
        if is_command_fence(line) {
            let mut cmd = String::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                if !cmd.is_empty() {
                    cmd.push('\n');
                }
                cmd.push_str(l);
            }
            let cmd = cmd.trim().to_string();
            return (!cmd.is_empty()).then_some(cmd);
        }
    }
    None
}

/// True if `line` opens a ```command fenced block (the run-this-command marker).
fn is_command_fence(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("```")
        .map(str::trim)
        .is_some_and(|info| info.eq_ignore_ascii_case("command"))
}

/// What to show of a *partial* (mid-stream) reply: hide a `<think>` block (even if it hasn't
/// closed yet — while the model is still reasoning, show nothing rather than the raw thought),
/// and don't show a half-written `file:` block (its opening fence line + body would look like
/// noise until complete). Used to render the live "typing" bubble as tokens arrive.
pub fn visible_so_far(partial: &str) -> String {
    // If a think block is open but not yet closed, the visible answer hasn't started.
    let lower = partial.to_ascii_lowercase();
    if let Some(open) = lower.find("<think>") {
        if !lower[open..].contains("</think>") {
            // Still thinking — show only whatever prose came BEFORE the <think> (usually none).
            return strip_control_tokens(&partial[..open]);
        }
    }
    let cleaned = strip_think(partial);
    // Cut everything from the first ```file: fence onward (a plan file being written) — we show
    // it as a proposal card once complete, not as raw streaming text.
    if let Some(idx) = cleaned.find("```file:") {
        return cleaned[..idx].trim().to_string();
    }
    cleaned.trim().to_string()
}

/// Remove a leading/embedded `<think>…</think>` reasoning block. Handles the common shape
/// (one block, possibly unterminated if the model ran out of tokens mid-think). Returns the
/// remaining visible text, trimmed.
fn strip_think(reply: &str) -> String {
    // EVERY block, not just the first. A reasoning model streams its thinking in many
    // deltas, and the backend tags each one, so a reply arrives as `<think>a</think>` +
    // `<think>b</think>` + … Stripping only the first left the rest visible, which is the
    // wall of "Wait — I'm Tiel-Coder… Actually, let me reconsider" the user saw.
    let mut out = reply.to_string();
    loop {
        let lower = out.to_ascii_lowercase();
        let Some(open) = lower.find("<think>") else {
            break;
        };
        let before = &out[..open];
        let after = if let Some(rel_close) = lower[open..].find("</think>") {
            out[open + rel_close + "</think>".len()..].to_string()
        } else {
            // Unterminated block (the model ran out of budget) — drop everything after it.
            String::new()
        };
        out = format!("{}{}", before.trim_end(), after);
    }
    // Strip control tokens AFTER removing the think blocks, so `/think` doesn't clobber
    // `</think>`.
    strip_control_tokens(&out)
}

/// Strip model control tokens that must never reach the chat bubble. The chat prompt appends a
/// `/no_think` (or `/think`) directive for Qwen3-style *thinking* models; the coder model in use
/// has no thinking mode, so it echoes the directive back verbatim. Tool/coder models also emit
/// `<tool_call>` (and `</tool_call>`) turn markers. None of these are content — remove them so
/// the user sees only the answer. Runs before `<think>` handling so both render paths are clean.
fn strip_control_tokens(reply: &str) -> String {
    let mut out = reply.to_string();
    // Bare reasoning directives, however the model spells them (leading slash, angle-bracketed).
    for tok in ["/no_think", "/think", "<no_think>", "<think_off>"] {
        out = out.replace(tok, "");
    }
    // Tool-call turn markers in any of the shapes coder models emit: <tool_call>, </tool_call>,
    // <tool_call|>. Strip the whole tag rather than trying to parse it — chat is prose-only.
    for tok in ["<tool_call|>", "</tool_call>", "<tool_call>"] {
        out = out.replace(tok, "");
    }
    out.trim().to_string()
}

/// If `line` opens a ```file:NAME fenced block, return NAME. Accepts optional whitespace and
/// a `.md`/any extension; the info string after the fence must start with `file:`.
fn fence_file_name(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix("```")?;
    let info = rest.trim();
    let name = info.strip_prefix("file:")?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Whether an investigation's answer actually proposes a CHANGE, and the instruction
/// an iterate run should be given if so.
///
/// The judgement is deliberately conservative and made from the answer's own words,
/// not by asking a second model. An investigation that concluded "this is working as
/// intended" or that only explained how something works must NOT sprout a button
/// offering to change it — an offer the user did not want is worse than no offer,
/// because clicking it edits their source.
///
/// So: the answer must name a file, and it must say something that reads as a
/// remedy. Both, not either.
pub fn proposed_fix(question: &str, answer: &str) -> Option<String> {
    let a = answer.trim();
    if a.is_empty() {
        return None;
    }
    if !names_a_file(a) || !proposes_a_change(a) {
        return None;
    }
    // The instruction an iterate run receives. It carries the ANSWER, because that is
    // where the file, the line and the remedy are; the question is context for why.
    Some(format!(
        "Apply this fix.\n\nThe user asked:\n\n{}\n\nThe investigation found:\n\n{}\n\nMake exactly \
         this change and nothing else. Do not refactor surrounding code.",
        question.trim(),
        a
    ))
}

/// Whether the text names something that looks like a source file.
fn names_a_file(text: &str) -> bool {
    const EXTS: &[&str] = &[
        ".rs", ".py", ".cs", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".c", ".h", ".cpp",
        ".toml", ".json", ".yaml", ".yml",
    ];
    EXTS.iter().any(|e| text.contains(e))
}

/// Whether the text reads as proposing a remedy rather than only explaining.
///
/// A word list, not a model call: this runs on every answered investigation, and a
/// second model call would be both slow and nondeterministic for a decision whose
/// only job is deciding whether to show a button.
fn proposes_a_change(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const CUES: &[&str] = &[
        "the fix",
        "fix is",
        "fix:",
        "should be",
        "should say",
        "needs to",
        "change ",
        "swap ",
        "replace ",
        "remove ",
        "add ",
        "instead of",
        "rather than",
        "incorrect",
        "wrong",
        "bug",
    ];
    CUES.iter().any(|c| lower.contains(c))
}

/// A runnable command found in an answer that was NOT the `Command` intent.
///
/// [`extract_command`] handles the deliberate case: the `Command` intent is told to emit
/// a ```command block, and the app offers it as a one-click Run. But an ordinary answer
/// often contains a command too — asked to launch the game, the model explained where
/// the instructions live and quoted `cargo run -p void_claim --release` as prose, which
/// is the right answer and completely unclickable.
///
/// So this looks for a command the model *mentioned* rather than one it was asked to
/// emit: a shell fence (```bash / ```sh / ```console / bare ```), or a
/// line that reads unmistakably like a build/run invocation. The bar is deliberately
/// high — see [`looks_runnable`] — because a wrong guess puts a button on the screen
/// that runs something the user did not ask for.
///
/// Returns `None` when [`extract_command`] already found one; the explicit block wins.
pub fn suggested_command(reply: &str) -> Option<String> {
    if extract_command(reply).is_some() {
        return None;
    }
    let cleaned = strip_think(reply);

    // 1. A fenced block whose info string is a shell language (or absent).
    let mut lines = cleaned.lines();
    while let Some(line) = lines.next() {
        let Some(info) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        let info = info.trim().to_ascii_lowercase();
        let shellish = info.is_empty()
            || matches!(
                info.as_str(),
                "bash" | "sh" | "shell" | "console" | "terminal" | "zsh"
            );
        if !shellish {
            continue;
        }
        for l in lines.by_ref() {
            if l.trim_start().starts_with("```") {
                break;
            }
            let candidate = l.trim().trim_start_matches("$ ").trim();
            if looks_runnable(candidate) {
                return Some(candidate.to_string());
            }
        }
    }

    // 2. A bare line of prose that is unmistakably a command. The model writes these
    //    when it is answering a question rather than proposing an action.
    cleaned
        .lines()
        .map(|l| l.trim().trim_start_matches("$ ").trim())
        .find(|l| looks_runnable(l))
        .map(str::to_string)
}

/// Whether a line is confidently a build/run command.
///
/// **Deliberately narrow.** This decides whether to put a Run button in front of the
/// user, and a button that runs the wrong thing is worse than no button: the cost of a
/// miss is they type the command themselves, the cost of a false positive is an
/// unexpected process. So it matches a short list of known build tools at the START of
/// the line, and rejects anything carrying shell punctuation that suggests a fragment,
/// a pipeline or prose wrapped around a command.
fn looks_runnable(line: &str) -> bool {
    const STARTERS: &[&str] = &[
        "cargo ", "npm ", "npx ", "pnpm ", "yarn ", "python ", "python3 ", "pytest", "go ",
        "dotnet ", "make ", "just ", "gradle ", "mvn ",
    ];
    let l = line.trim();
    // A command is one line and not a paragraph.
    if l.is_empty() || l.len() > 200 || l.contains(". ") {
        return false;
    }
    // Prose about a command ("run `cargo test`") is not a command.
    if l.contains('`') {
        return false;
    }
    // Pipelines, redirects and chains are not offered: too easy to get wrong, and the
    // sandbox would likely refuse them anyway.
    if l.contains('|') || l.contains('>') || l.contains("&&") || l.contains(';') {
        return false;
    }
    STARTERS.iter().any(|p| {
        l.strip_prefix(p)
            .map(|rest| !rest.trim().is_empty())
            .unwrap_or_else(|| l == p.trim())
    })
}
