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

/// What the user's latest turn is asking for — decided by a fast, grammar-constrained
/// classification call to the model (NOT by string-matching the reply). The generate step is
/// then given one unambiguous instruction (and, for file-producing intents, a grammar that forces
/// the right `file:` block) so the model can't misroute or forget the fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatIntent {
    /// A question about the plan or the open file → answer in prose, no file.
    Question,
    /// Add/remove/reorder whole-project backlog items → a `TODO.md` block.
    TodoEdit,
    /// Change the project overview/architecture → a `README.md` block.
    ReadmeEdit,
    /// Design a feature or investigate a file → a `PLAN-<slug>.md` block.
    FeaturePlan,
    /// A request to change source code → prose telling the user to comment on the code lines
    /// (this chat can't edit source).
    CodeChange,
}

impl ChatIntent {
    /// The classifier's label token for this intent (the single word the grammar allows).
    fn token(self) -> &'static str {
        match self {
            ChatIntent::Question => "question",
            ChatIntent::TodoEdit => "todo_edit",
            ChatIntent::ReadmeEdit => "readme_edit",
            ChatIntent::FeaturePlan => "feature_plan",
            ChatIntent::CodeChange => "code_change",
        }
    }

    /// Every intent, for building the classifier grammar / parsing its reply.
    fn all() -> [ChatIntent; 5] {
        [
            ChatIntent::Question,
            ChatIntent::TodoEdit,
            ChatIntent::ReadmeEdit,
            ChatIntent::FeaturePlan,
            ChatIntent::CodeChange,
        ]
    }

    /// Parse the classifier's (grammar-constrained) reply back into an intent. The grammar
    /// guarantees one of the tokens, but we match leniently and default to `Question` (the safe,
    /// prose-only intent) if anything unexpected comes back.
    pub fn parse(reply: &str) -> ChatIntent {
        let t = reply.trim().to_ascii_lowercase();
        ChatIntent::all()
            .into_iter()
            .find(|i| t.contains(i.token()))
            .unwrap_or(ChatIntent::Question)
    }
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
    /// The file open in the code view — `(name, contents)` — so a question like "what does
    /// this do?" is answered against what the user is actually looking at. `None` when no
    /// file is open. Injected (head-clipped) into the system prompt.
    open_file: Option<(String, String)>,
}

/// How much of an open file to inject into the system prompt. A small model's window is
/// tight ([`crate::chat`] keeps the plan on disk for the same reason), so a long file is
/// head-clipped: the top of a source file (imports, types, signatures) is what a question
/// usually needs, and the note tells the model the rest was cut.
const OPEN_FILE_MAX_LINES: usize = 200;

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
            open_file: None,
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

    /// Set (or clear) the file open in the code view, so the next request can answer a
    /// question against what the user is looking at. Pass `None` when no file is open, or a
    /// `(name, contents)` pair for the current file. Contents are head-clipped at inject time.
    pub fn set_open_file(&mut self, file: Option<(String, String)>) {
        self.open_file = file;
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
    /// Build the fast classification call: given the conversation so far, ask the model which of
    /// the [`ChatIntent`] cases the latest user turn is. The reply is constrained by a GBNF
    /// grammar to exactly ONE intent token, so the result needs no parsing heuristics — the model
    /// classifies, we don't guess. Tiny (a handful of tokens), so it's milliseconds on the 30B.
    pub fn classify_request(&self) -> GenerateRequest {
        let last_user = self
            .turns
            .iter()
            .rev()
            .find(|m| matches!(m.role, dc_model::Role::User))
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let open = self
            .open_file
            .as_ref()
            .map(|(n, _)| format!(" The user currently has the file `{n}` open in the code view."))
            .unwrap_or_default();
        let sys = format!(
            "You classify a user's message in a project-planning chat into exactly ONE intent. \
             Reply with ONLY the intent word, nothing else.\n\
             • question — asking about the plan/code, wants an answer or discussion (incl. \
             \"what do you think of this file?\", \"anything you'd change?\"). Reviewing or \
             critiquing a file is a question UNLESS they ask to write the result down.\n\
             • todo_edit — add/remove/reorder items in the whole-project TODO backlog.\n\
             • readme_edit — change the project overview/architecture in the README.\n\
             • feature_plan — design a feature, or WRITE DOWN / MAKE A PLAN to investigate or \
             improve something (\"make a plan to…\", \"plan out adding X\", \"write up how we'd \
             fix this file\").\n\
             • code_change — asking to actually edit source code (\"rename X\", \"fix this \
             function\", \"change the code\").{open}"
        );
        let messages = vec![
            Message::system(sys),
            Message::user(format!("Message to classify:\n{last_user}\n\nIntent:")),
        ];
        let mut req = GenerateRequest::new(messages);
        req.temperature = 0.0;
        req.max_tokens = 8;
        req.constraint = Some(dc_model::OutputConstraint::Grammar(intent_grammar()));
        req
    }

    /// Build the generate call for a classified `intent`. The system prompt is tailored to the
    /// single known intent (no four-case disambiguation for the model to get wrong), and
    /// file-producing intents attach a GBNF grammar that FORCES the output into the right
    /// `file:<name>` block — so the model structurally cannot forget the fence or pick the wrong
    /// target file. `think` controls the reasoning budget as before.
    pub fn request(&self, think: bool, intent: ChatIntent) -> GenerateRequest {
        let mut sys = self.system_prompt();
        sys.push_str(&intent_instruction(intent, self.plan_slug()));
        // Qwen3-style directive: /no_think = answer directly, /think = reason first.
        sys.push_str(if think { "/think\n" } else { "/no_think\n" });

        let mut messages = vec![Message::system(sys)];
        let start = self.turns.len().saturating_sub(KEEP_TURNS);
        messages.extend(self.turns[start..].iter().cloned());
        let mut req = GenerateRequest::new(messages);
        req.temperature = 0.4;
        // Thinking needs room to reach a conclusion; fast mode stays tight but must still fit a
        // multi-section feature plan without truncating mid-block. The model stops early on short
        // answers, so the larger ceiling only costs tokens when it genuinely writes a plan.
        req.max_tokens = if think { 2400 } else { 1200 };
        req
    }

    /// The plan slug for a feature-plan target, derived from the OPEN FILE's name (e.g.
    /// `Assets/Scripts/SolarPanelTracker.cs` → `solar-panel-tracker`) so a "make a plan for this
    /// file" lands as `PLAN-solar-panel-tracker.md`. Falls back to `feature` when no file is open.
    fn plan_slug(&self) -> String {
        let stem = self
            .open_file
            .as_ref()
            .and_then(|(name, _)| {
                std::path::Path::new(name)
                    .file_stem()
                    .and_then(|s| s.to_str())
            })
            .unwrap_or("feature");
        slugify(stem)
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
        // The specific per-intent directive is appended by `intent_instruction` (the intent is
        // classified by a separate call), so the base prompt only states the shared file-block
        // format and the never-write-source rule.
        s.push_str(
            "When you propose a plan file, output its FULL new contents in a fenced block whose \
             info string is `file:<name>`, e.g.\n\
             ```file:TODO.md\n- [ ] first task\n```\n\
             Always put a one-line prose lead-in BEFORE any file block. You cannot edit source \
             code here — only the plan (README/TODO/PLAN docs).\n\n",
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
        // The file the user is currently looking at, head-clipped. A question like "what does
        // this do?" or "add error handling here" refers to THIS file — inject it so the model
        // answers against what's on screen rather than guessing.
        if let Some((name, body)) = &self.open_file {
            if !body.trim().is_empty() {
                let (clipped, cut) = clip_lines(body, OPEN_FILE_MAX_LINES);
                s.push_str(&format!(
                    "=== file open in the code view: {name} ===\n\
                     (This is what the user is looking at. Questions like \"what does this do?\" \
                     or \"how would I change this?\" refer to this file.)\n",
                ));
                s.push_str(clipped.trim_end());
                if cut {
                    s.push_str("\n… (file truncated — only the first portion is shown)");
                }
                s.push_str("\n\n");
            }
        }
        s
    }
}

/// Head-clip `body` to at most `max_lines` lines. Returns the clipped text and whether any
/// lines were dropped, so the caller can add a truncation note.
fn clip_lines(body: &str, max_lines: usize) -> (String, bool) {
    let mut out = String::new();
    let mut n = 0;
    for line in body.lines().take(max_lines) {
        out.push_str(line);
        out.push('\n');
        n += 1;
    }
    let cut = body.lines().nth(n).is_some();
    (out, cut)
}

/// The GBNF grammar for the intent classifier: the whole output must be exactly one intent
/// token. This makes the classification unforgeable — the model can only emit a valid label, so
/// [`ChatIntent::parse`] never has to guess.
fn intent_grammar() -> String {
    let alts = ChatIntent::all()
        .into_iter()
        .map(|i| format!("\"{}\"", i.token()))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("root ::= {alts}")
}

/// The per-intent generate instruction, appended to the base system prompt once the intent is
/// known. Because the intent is already decided, each instruction is unambiguous — no four-case
/// menu for the model to misread. `slug` is the plan filename slug for a feature plan.
fn intent_instruction(intent: ChatIntent, slug: String) -> String {
    match intent {
        ChatIntent::Question => "INTENT: the user asked a QUESTION or wants a review/discussion. \
             Answer in PLAIN PROSE only. Do NOT output any file block."
            .to_string(),
        ChatIntent::TodoEdit => "INTENT: update the whole-project backlog. Output the FULL new \
             contents of `TODO.md` in a ```file:TODO.md block, after a one-line prose lead-in."
            .to_string(),
        ChatIntent::ReadmeEdit => "INTENT: update the project overview. Output the FULL new \
             contents of `README.md` in a ```file:README.md block, after a one-line prose lead-in."
            .to_string(),
        ChatIntent::FeaturePlan => format!(
            "INTENT: write a FEATURE PLAN (design/investigation doc). Output it as a \
             ```file:PLAN-{slug}.md block, after a one-line prose lead-in (e.g. \"Here's a \
             plan:\"). Sections: `## Plan: <title>`, then **Approach** (2–3 sentences), **Files \
             to touch** (a bullet per file, mark new files `(new)`), **Steps** (numbered build \
             order), **Risks**. This is a DESIGN doc — describe the changes, do NOT write source \
             code. Do NOT touch TODO.md or README.md."
        ),
        ChatIntent::CodeChange => "INTENT: the user asked to change SOURCE CODE, which you cannot \
             do from this chat. Reply in PROSE telling them to select the lines in the code view \
             on the right and comment on them. Do NOT edit TODO.md or README.md."
            .to_string(),
    }
}

/// Kebab-case a name into a filename slug, keeping it short. E.g. "SolarPanelTracker" →
/// "solar-panel-tracker" (camelCase split), "Solar Panel" → "solar-panel".
fn slugify(name: &str) -> String {
    // Split on non-alphanumeric AND camelCase humps, lowercase, join with '-'.
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
                words.push(std::mem::take(&mut cur)); // camelCase boundary
            }
            cur.push(c.to_ascii_lowercase());
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let slug = words
        .into_iter()
        .filter(|w| !w.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "feature".to_string()
    } else {
        slug
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
    let lower = reply.to_ascii_lowercase();
    let Some(open) = lower.find("<think>") else {
        // No think block — still strip any leaked control tokens from the plain reply.
        return strip_control_tokens(reply);
    };
    let before = &reply[..open];
    // Find the matching close after the open tag.
    let after = if let Some(rel_close) = lower[open..].find("</think>") {
        &reply[open + rel_close + "</think>".len()..]
    } else {
        // Unterminated think block (model ran out of budget) — drop everything after <think>.
        ""
    };
    // Strip control tokens AFTER removing the think block, so `/think` doesn't clobber `</think>`.
    strip_control_tokens(&format!("{}{}", before.trim_end(), after))
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
        let req = c.request(false, ChatIntent::Question);
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
    fn classify_request_is_grammar_constrained_to_the_intent_tokens() {
        // The classifier must FORCE one intent word via GBNF — no free-text to misparse.
        let mut c = Conversation::open("# X", "- a");
        c.user_turn("can you make a plan to investigate these issues?");
        let req = c.classify_request();
        match req.constraint {
            Some(dc_model::OutputConstraint::Grammar(g)) => {
                assert!(g.contains("feature_plan"), "grammar lists intents: {g}");
                assert!(g.contains("question"), "grammar lists intents: {g}");
            }
            other => panic!("expected a grammar constraint, got {other:?}"),
        }
    }

    #[test]
    fn feature_plan_intent_targets_a_plan_file_named_after_the_open_file() {
        // A feature-plan for an open `SolarPanelTracker.cs` → PLAN-solar-panel-tracker.md,
        // NOT a TODO edit (the reported bug).
        let mut c = Conversation::open("# X", "- a");
        c.set_open_file(Some(("Assets/Scripts/SolarPanelTracker.cs".into(), "class X {}".into())));
        c.user_turn("make a plan to investigate these");
        let sys = c.request(false, ChatIntent::FeaturePlan).messages[0]
            .content
            .clone();
        assert!(sys.contains("PLAN-solar-panel-tracker.md"), "slug from open file: {sys}");
        assert!(sys.to_lowercase().contains("do not touch todo"), "TODO off-limits: {sys}");
    }

    #[test]
    fn intent_parse_maps_tokens_and_defaults_to_question() {
        assert_eq!(ChatIntent::parse("feature_plan"), ChatIntent::FeaturePlan);
        assert_eq!(ChatIntent::parse("  todo_edit\n"), ChatIntent::TodoEdit);
        assert_eq!(ChatIntent::parse("gibberish"), ChatIntent::Question);
    }

    #[test]
    fn fast_mode_budget_fits_a_feature_plan() {
        // A multi-section feature plan can't land in the old 700-token fast budget.
        assert!(Conversation::open("", "").request(false, ChatIntent::FeaturePlan).max_tokens >= 1200);
    }

    #[test]
    fn open_file_is_injected_into_the_prompt_and_head_clipped() {
        let mut c = Conversation::open("# App", "- [ ] x");
        let body: String = (1..=500).map(|n| format!("line {n}\n")).collect();
        c.set_open_file(Some(("src/water.rs".to_string(), body)));
        let sys = c.request(false, ChatIntent::Question).messages[0].content.clone();
        assert!(sys.contains("file open in the code view: src/water.rs"), "name shown");
        assert!(sys.contains("line 1\n"), "head of file present");
        assert!(!sys.contains("line 500"), "tail clipped past the cap");
        assert!(sys.contains("truncated"), "truncation noted");
    }

    #[test]
    fn open_file_none_injects_nothing() {
        let mut c = Conversation::open("", "");
        c.set_open_file(None);
        assert!(!c.request(false, ChatIntent::Question).messages[0]
            .content
            .contains("file open in the code view"));
        // An empty/whitespace file is treated as nothing too.
        c.set_open_file(Some(("empty.rs".to_string(), "   \n".to_string())));
        assert!(!c.request(false, ChatIntent::Question).messages[0]
            .content
            .contains("file open in the code view"));
    }

    #[test]
    fn parse_reply_carries_a_feature_plan_file_block() {
        // The generic file:<name> plumbing already routes a PLAN-*.md doc through — no app change.
        let reply = "Here's a plan for lakes:\n\
             ```file:PLAN-lakes.md\n## Plan: lakes\n**Approach:** flood-fill basins.\n```\n\
             Want me to break this into TODO items?";
        let (prose, files) = parse_reply(reply);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "PLAN-lakes.md");
        assert!(files[0].content.contains("## Plan: lakes"));
        assert!(prose.contains("plan for lakes"));
        assert!(!prose.contains("flood-fill"), "plan body not left in prose");
    }

    #[test]
    fn history_is_capped_to_keep_the_window_small() {
        let mut c = Conversation::open("", "");
        for i in 0..40 {
            c.user_turn(&format!("msg {i}"));
            c.record_reply(&format!("reply {i}"));
        }
        let req = c.request(false, ChatIntent::Question);
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
    fn parse_reply_strips_leaked_control_tokens() {
        // The coder model has no thinking mode, so it echoes the /no_think directive, and it
        // emits <tool_call> turn markers. Neither is content — both must be gone from the bubble.
        let reply = "/no_think\nHere's the plan.\n<tool_call>";
        let (prose, _files) = parse_reply(reply);
        assert!(!prose.contains("/no_think"), "directive stripped: {prose:?}");
        assert!(!prose.contains("tool_call"), "tool marker stripped: {prose:?}");
        assert!(prose.contains("Here's the plan"), "answer kept: {prose:?}");
    }

    #[test]
    fn parse_reply_drops_an_unterminated_think_block() {
        // If the model runs out of budget mid-think, don't dump the partial reasoning.
        let reply = "<think>reasoning that never closes and fills the whole reply";
        let (prose, _files) = parse_reply(reply);
        assert!(prose.is_empty(), "unterminated think dropped: {prose:?}");
    }

    #[test]
    fn question_intent_forbids_a_file_block() {
        // A classified QUESTION must instruct prose-only — no TODO/README rewrite for "what's
        // next?" (the original mis-route bug, now decided by the classifier not the model).
        let sys = Conversation::open("# X", "- a")
            .request(false, ChatIntent::Question)
            .messages[0]
            .content
            .to_lowercase();
        assert!(sys.contains("prose"), "question → prose: {sys}");
        assert!(sys.contains("do not output any file"), "no file block: {sys}");
    }

    #[test]
    fn todo_intent_targets_todo_and_code_change_targets_neither() {
        let todo = Conversation::open("# X", "- a")
            .request(false, ChatIntent::TodoEdit)
            .messages[0]
            .content
            .clone();
        assert!(todo.contains("file:TODO.md"), "todo edit → TODO block: {todo}");
        let code = Conversation::open("# X", "- a")
            .request(false, ChatIntent::CodeChange)
            .messages[0]
            .content
            .to_lowercase();
        assert!(code.contains("cannot"), "code change refused in chat: {code}");
        assert!(code.contains("comment"), "steers to code-view comment: {code}");
    }

    #[test]
    fn thinking_stays_enabled_by_prompt_but_is_channeled_to_tags() {
        // The system prompt should ENCOURAGE reasoning in <think> tags, not forbid it.
        let c = Conversation::open("", "");
        let sys = &c.request(false, ChatIntent::Question).messages[0].content;
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
