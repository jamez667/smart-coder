//! [`Conversation`] — the plan-first chat engine. A multi-turn planning conversation with
//! the model, built on `dc_model`'s one primitive: a growing `Vec<Message>` sent to
//! `backend.generate`. The agent's job here is to *plan* (build up README.md / TODO.md as
//! real files), not to write source code — the system prompt enforces that.
//!
//! Pure/host-testable: no backend call and no iced types live here. The worker
//! ([`crate::chat_session`]) owns the actual `generate` call; this module owns *what to
//! send* (history + a mode-shaped system prompt) and *how to read the reply* (extracting the
//! `file:<name>` plan-file blocks the model proposes).

use dc_model::{GenerateRequest, Message};

/// Which planning posture the conversation opens in, decided from what's already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Empty/near-empty project — no README and no TODO. The agent asks what to build and
    /// co-authors the plan files from scratch.
    Scratch,
    /// An existing project with a README and/or TODO — the agent reads them and continues
    /// from where the user left off.
    Existing,
}

/// A plan-file the assistant proposed in a reply (a ```file:NAME block). The app shows it in
/// the code view and writes it on the user's Apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedFile {
    /// The target filename, workspace-relative (e.g. `TODO.md`).
    pub name: String,
    /// The full proposed contents.
    pub content: String,
}

/// One turn shown in the chat thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Speaker,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    You,
    Agent,
    /// A debug echo (the raw prompt sent to the model), shown only in debug mode.
    Debug,
}

/// How many most-recent user/assistant turns to keep verbatim in the request. The system
/// prompt (with the current plan files) is always kept; older chatter is dropped so a small
/// model's window never overflows. The plan lives on disk, so dropping old turns is safe —
/// the files carry the state, not the transcript.
const KEEP_TURNS: usize = 12;

/// A planning conversation: the mode, the running transcript, and the current plan-file
/// contents (re-injected into the system prompt each request so the model always plans
/// against the real files).
#[derive(Debug, Clone)]
pub struct Conversation {
    mode: Mode,
    /// The user/assistant transcript (no system message — that's rebuilt per request).
    turns: Vec<Message>,
    /// Current README.md contents (empty if none), injected into the system prompt.
    readme: String,
    /// Current TODO.md contents (empty if none), injected into the system prompt.
    todo: String,
}

impl Conversation {
    /// Open a conversation. `readme`/`todo` are the current on-disk contents (empty = absent);
    /// the mode is [`Mode::Scratch`] when both are empty, else [`Mode::Existing`].
    pub fn open(readme: &str, todo: &str) -> Self {
        let mode = if readme.trim().is_empty() && todo.trim().is_empty() {
            Mode::Scratch
        } else {
            Mode::Existing
        };
        Self {
            mode,
            turns: Vec::new(),
            readme: readme.to_string(),
            todo: todo.to_string(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The assistant's opening line — shown before the user says anything, so the app isn't a
    /// blank box. Scratch invites the build; existing acknowledges the plan and asks what next.
    pub fn opening_line(&self) -> String {
        match self.mode {
            Mode::Scratch => "What do you want to build? Tell me the idea and we'll shape a \
                 README and a TODO together — no code yet, just the plan."
                .to_string(),
            Mode::Existing => {
                "I've read your README and TODO. Where do you want to pick up — a TODO item, a \
                 new direction, or a question about the project?"
                    .to_string()
            }
        }
    }

    /// Append a user turn.
    pub fn user_turn(&mut self, text: &str) {
        self.turns.push(Message::user(text));
    }

    /// Append the assistant's reply.
    pub fn record_reply(&mut self, content: &str) {
        self.turns.push(Message::assistant(content));
    }

    /// Refresh the plan-file contents (call after an Apply writes README/TODO, so the next
    /// request reflects the new files).
    pub fn set_plan_files(&mut self, readme: &str, todo: &str) {
        self.readme = readme.to_string();
        self.todo = todo.to_string();
    }

    /// Update just the README snapshot the system prompt injects (e.g. from a proposed-but-
    /// not-yet-applied file, so follow-up turns plan against what was proposed).
    pub fn set_readme(&mut self, readme: &str) {
        self.readme = readme.to_string();
    }

    /// Update just the TODO snapshot the system prompt injects.
    pub fn set_todo(&mut self, todo: &str) {
        self.todo = todo.to_string();
    }

    /// Build the request for the next model call: the mode-shaped system prompt (with the
    /// current plan files) plus the last [`KEEP_TURNS`] transcript turns.
    ///
    /// `think` chooses the reasoning mode. This 8B doesn't reliably self-tag its reasoning,
    /// so:
    ///  • `think = false` (the fast default) appends `/no_think` — the model answers with the
    ///    conclusion directly, no rambling, small token budget.
    ///  • `think = true` lets it reason (`/think`) with a larger budget so it can finish; the
    ///    app hides any `<think>` block from the chat bubble.
    pub fn request(&self, think: bool) -> GenerateRequest {
        let mut sys = self.system_prompt();
        // Qwen3-style directive: /no_think = answer directly, /think = reason first.
        sys.push_str(if think { "/think\n" } else { "/no_think\n" });

        let mut messages = vec![Message::system(sys)];
        let start = self.turns.len().saturating_sub(KEEP_TURNS);
        messages.extend(self.turns[start..].iter().cloned());
        let mut req = GenerateRequest::new(messages);
        req.temperature = 0.4;
        // Thinking needs room to reach a conclusion; fast mode stays tight.
        req.max_tokens = if think { 2400 } else { 700 };
        req
    }

    /// The system prompt — the design lever. Sets the planning posture, forbids writing code,
    /// and specifies the ```file:NAME block format for plan-file proposals. Injects the
    /// current README/TODO so the model always plans against the real files.
    fn system_prompt(&self) -> String {
        let mut s = String::new();
        s.push_str(
            "You are a planning partner inside a desktop coding app. You and the user shape a \
             project's PLAN together — its README (what it is / architecture) and its TODO \
             (the backlog). Be concise and fast: short, direct replies, one question at a time, \
             no walls of text. Do NOT write source code here — this is planning, not \
             implementation; the user runs a separate build step for code.\n\n",
        );
        match self.mode {
            Mode::Scratch => s.push_str(
                "This is a NEW, empty project. Ask what they want to build, then help draft a \
                 README and a TODO. Propose them as files (see below) once there's enough to \
                 write down.\n\n",
            ),
            Mode::Existing => s.push_str(
                "This is an EXISTING project. Its current plan files are below. Continue from \
                 them — refine the TODO, discuss the next item, or answer questions. Propose \
                 updated files only when the plan actually changes.\n\n",
            ),
        }
        s.push_str(
            "CRITICAL — three cases:\n\
             • A CODE-CHANGE request (\"shorten this comment\", \"rename X\", \"fix this \
             function\", \"change the code to…\") → you CANNOT edit source code from this chat; \
             it only edits the plan (README/TODO). Reply in prose telling the user: to change \
             code, select the lines in the code view on the right and comment on them. Do NOT \
             edit the TODO/README for a code request.\n\
             • A QUESTION about the plan (\"what's next?\", \"which is smallest?\", \"why did \
             we…?\") → reply in PLAIN PROSE only. No file block.\n\
             • A PLAN-EDIT request (\"add X to the todo\", \"remove Y\", \"update the readme \
             to…\", \"reprioritize\") → output the FULL new contents of the affected plan file \
             in a fenced block whose info string is `file:<name>`, e.g.\n\
             ```file:TODO.md\n- [ ] first task\n- [ ] second task\n```\n\
             Default to PROSE. Only produce a file block for a clear PLAN-file change (README/\
             TODO). When unsure, answer in prose and ask. Never reply with a file block and no \
             prose.\n\n",
        );
        // Channel any reasoning into <think> tags so the user sees only the conclusion —
        // thinking is welcome (it makes the plan better), but it must not be the visible
        // reply. The app hides the <think> block from the chat bubble.
        s.push_str(
            "If you need to reason, put it inside <think>…</think> tags FIRST, then give your \
             short answer AFTER the closing tag. Never let raw reasoning be the visible reply.\n\n",
        );
        if !self.readme.trim().is_empty() {
            s.push_str("=== current README.md ===\n");
            s.push_str(self.readme.trim());
            s.push_str("\n\n");
        }
        if !self.todo.trim().is_empty() {
            s.push_str("=== current TODO.md ===\n");
            s.push_str(self.todo.trim());
            s.push_str("\n\n");
        }
        s
    }
}

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
            });
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    (prose.trim().to_string(), files)
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
            return partial[..open].trim().to_string();
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
    let lower = reply.to_ascii_lowercase();
    let Some(open) = lower.find("<think>") else {
        return reply.to_string();
    };
    let before = &reply[..open];
    // Find the matching close after the open tag.
    let after = if let Some(rel_close) = lower[open..].find("</think>") {
        &reply[open + rel_close + "</think>".len()..]
    } else {
        // Unterminated think block (model ran out of budget) — drop everything after <think>.
        ""
    };
    format!("{}{}", before.trim_end(), after).trim().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_scratch_only_when_both_files_absent() {
        assert_eq!(Conversation::open("", "").mode(), Mode::Scratch);
        assert_eq!(Conversation::open("# Readme", "").mode(), Mode::Existing);
        assert_eq!(Conversation::open("", "- todo").mode(), Mode::Existing);
        assert_eq!(Conversation::open("  ", "\n").mode(), Mode::Scratch);
    }

    #[test]
    fn opening_line_differs_by_mode() {
        assert!(Conversation::open("", "")
            .opening_line()
            .to_lowercase()
            .contains("build"));
        assert!(Conversation::open("# x", "")
            .opening_line()
            .to_lowercase()
            .contains("pick up"));
    }

    #[test]
    fn request_carries_system_prompt_plus_turns_and_injects_plan_files() {
        let mut c = Conversation::open("# My Game\nA city sim.", "- [ ] add lakes");
        c.user_turn("what's left to do?");
        let req = c.request(false);
        assert_eq!(req.messages[0].role, dc_model::Role::System);
        let sys = &req.messages[0].content;
        assert!(sys.contains("My Game"), "README injected: {sys}");
        assert!(sys.contains("add lakes"), "TODO injected: {sys}");
        assert!(sys.to_lowercase().contains("plan"), "planning posture set");
        // The user turn is present after the system message.
        assert!(req
            .messages
            .iter()
            .any(|m| m.role == dc_model::Role::User && m.content.contains("what's left")));
    }

    #[test]
    fn history_is_capped_to_keep_the_window_small() {
        let mut c = Conversation::open("", "");
        for i in 0..40 {
            c.user_turn(&format!("msg {i}"));
            c.record_reply(&format!("reply {i}"));
        }
        let req = c.request(false);
        // system + at most KEEP_TURNS transcript messages.
        assert!(
            req.messages.len() <= KEEP_TURNS + 1,
            "history capped, got {}",
            req.messages.len()
        );
        // The MOST RECENT turns are kept (msg 39), the oldest (msg 0) dropped.
        let joined: String = req.messages.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("msg 39"), "recent kept");
        assert!(!joined.contains("msg 0\n") && !joined.contains("\"msg 0\""));
    }

    #[test]
    fn parse_reply_extracts_file_blocks_and_leaves_prose() {
        let reply = "Sure — here's the updated backlog:\n\
             ```file:TODO.md\n- [ ] add lakes\n- [ ] rail\n```\n\
             Want me to prioritize any of these?";
        let (prose, files) = parse_reply(reply);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "TODO.md");
        assert_eq!(files[0].content, "- [ ] add lakes\n- [ ] rail");
        assert!(prose.contains("here's the updated backlog"));
        assert!(prose.contains("prioritize"));
        assert!(!prose.contains("add lakes"), "file body not left in prose");
    }

    #[test]
    fn parse_reply_leaves_a_plain_code_fence_inline() {
        // A non-file fenced block (an example) is NOT a plan file — it stays in the prose.
        let reply = "You'd call it like:\n```\ncargo run\n```\nmakes sense?";
        let (prose, files) = parse_reply(reply);
        assert!(files.is_empty(), "no file blocks");
        assert!(prose.contains("cargo run"), "plain fence kept inline");
    }

    #[test]
    fn parse_reply_strips_a_think_block_but_keeps_the_answer() {
        // Thinking is welcome, but the <think> block must not show — only the conclusion.
        let reply = "<think>let me consider the options... maybe lakes</think>\n\
             I'd add lakes next — small and visual.";
        let (prose, _files) = parse_reply(reply);
        assert!(
            !prose.contains("let me consider"),
            "reasoning hidden: {prose}"
        );
        assert!(prose.contains("add lakes next"), "answer kept: {prose}");
    }

    #[test]
    fn parse_reply_drops_an_unterminated_think_block() {
        // If the model runs out of budget mid-think, don't dump the partial reasoning.
        let reply = "<think>reasoning that never closes and fills the whole reply";
        let (prose, _files) = parse_reply(reply);
        assert!(prose.is_empty(), "unterminated think dropped: {prose:?}");
    }

    #[test]
    fn prompt_separates_answering_a_question_from_editing() {
        // The system prompt must tell the model: questions → prose; edit requests → file
        // block. This is the fix for "what's next?" wrongly rewriting the TODO.
        let sys = Conversation::open("# X", "- a").request(false).messages[0]
            .content
            .to_lowercase();
        assert!(sys.contains("question"), "distinguishes questions: {sys}");
        assert!(
            sys.contains("prose") && sys.contains("edit"),
            "prose-vs-edit rule present: {sys}"
        );
    }

    #[test]
    fn thinking_stays_enabled_by_prompt_but_is_channeled_to_tags() {
        // The system prompt should ENCOURAGE reasoning in <think> tags, not forbid it.
        let c = Conversation::open("", "");
        let sys = &c.request(false).messages[0].content;
        assert!(
            sys.contains("<think>"),
            "prompt channels reasoning into tags: {sys}"
        );
    }

    #[test]
    fn visible_so_far_hides_open_think_and_partial_file_blocks() {
        // Mid-think (unclosed) → nothing visible yet.
        assert_eq!(visible_so_far("<think>reasoning still going"), "");
        // Think closed → the answer after it shows.
        assert_eq!(
            visible_so_far("<think>done</think>Add lakes next."),
            "Add lakes next."
        );
        // A half-written file block is cut off (shown as a card once complete).
        assert_eq!(
            visible_so_far("Here's the todo:\n```file:TODO.md\n- [ ] a"),
            "Here's the todo:"
        );
        // Plain partial prose streams through as-is.
        assert_eq!(visible_so_far("I'd sugg"), "I'd sugg");
    }

    #[test]
    fn parse_reply_handles_two_file_blocks() {
        let reply = "```file:README.md\n# Game\n```\nand\n```file:TODO.md\n- [ ] x\n```";
        let (_prose, files) = parse_reply(reply);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "README.md");
        assert_eq!(files[1].name, "TODO.md");
    }
}
