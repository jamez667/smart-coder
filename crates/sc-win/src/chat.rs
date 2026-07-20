//! [`Conversation`] — the plan-first chat engine. A multi-turn planning conversation with
//! the model, built on `sc_model`'s one primitive: a growing `Vec<Message>` sent to
//! `backend.generate`. The agent's job here is to *plan* (build up README.md / TODO.md as
//! real files), not to write source code — the system prompt enforces that.
//!
//! Pure/host-testable: no backend call and no iced types live here. The worker
//! ([`crate::chat_session`]) owns the actual `generate` call; this module owns *what to
//! send* (history + a mode-shaped system prompt) and *how to read the reply* (extracting the
//! `file:<name>` plan-file blocks the model proposes).

use sc_model::{GenerateRequest, Message};

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
    /// GENERIC conversation not about this project ("hello", "how are you", "what can you
    /// do?") → answer in prose with a MINIMAL prompt: no README/TODO/open-file, no planning
    /// boilerplate. Keeps a plain greeting from dragging the whole project context along.
    Chat,
    /// A question ABOUT this project/plan/open file → answer in prose, with the relevant
    /// context (open file) but no file block.
    Question,
    /// Add/remove/reorder whole-project backlog items → a `TODO.md` block.
    TodoEdit,
    /// Change the project overview/architecture → a `README.md` block.
    ReadmeEdit,
    /// Spec a FRESH feature (not tied to the backlog) → a `PLAN-<slug>.md` spec. Gets the
    /// README for project context, but NOT the TODO (a fresh feature spec doesn't need the
    /// backlog).
    FeaturePlan,
    /// Spec/plan something FROM the backlog ("plan the next TODO item", "what's next on the
    /// backlog") → the same `PLAN-<slug>.md` spec, but WITH the TODO injected. Split from
    /// `FeaturePlan` so a plain "plan feature X" doesn't drag the whole backlog into context.
    PlanFromTodo,
    /// A request to change source code → prose telling the user to comment on the code lines
    /// (this chat can't edit source).
    CodeChange,
    /// A request to RUN something (build/launch/test/a shell command) → emit a ```command
    /// block the app offers as a one-click Run in the integrated terminal. This is the intent
    /// that turns "start the windows client" into `cargo run -p sc-win` instead of a PLAN.
    Command,
}

impl ChatIntent {
    /// The classifier's label token for this intent (the single word the grammar allows).
    fn token(self) -> &'static str {
        match self {
            ChatIntent::Chat => "chat",
            ChatIntent::Question => "question",
            ChatIntent::TodoEdit => "todo_edit",
            ChatIntent::ReadmeEdit => "readme_edit",
            ChatIntent::FeaturePlan => "feature_plan",
            ChatIntent::PlanFromTodo => "plan_from_todo",
            ChatIntent::CodeChange => "code_change",
            ChatIntent::Command => "command",
        }
    }

    /// Every intent, for building the classifier grammar / parsing its reply.
    fn all() -> [ChatIntent; 8] {
        [
            ChatIntent::Chat,
            ChatIntent::Question,
            ChatIntent::TodoEdit,
            ChatIntent::ReadmeEdit,
            ChatIntent::FeaturePlan,
            ChatIntent::PlanFromTodo,
            ChatIntent::CodeChange,
            ChatIntent::Command,
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
    /// The project's file paths (workspace-relative, `/`-separated), noise-filtered. Injected
    /// into a FEATURE-PLAN prompt so "Files to touch" names REAL paths instead of hallucinated
    /// ones — the file tree is cheap (paths only, not contents), unlike a full-file dump.
    file_tree: Vec<String>,
}

/// Max file paths to inject into a feature-plan prompt. A big repo could have thousands of
/// files; capping keeps a small model's window safe while still grounding "Files to touch" for
/// the vast majority of projects. Paths are ~40 chars, so 400 ≈ 16k chars — comfortably within
/// budget alongside README/TODO.
const FILE_TREE_MAX: usize = 400;

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
            file_tree: Vec::new(),
        }
    }

    /// Set the project's file paths (workspace-relative), injected into a feature-plan prompt so
    /// the plan references REAL files. Cheap (paths only). The app refreshes this from its tree
    /// cache when the plan conversation opens / a plan turn is sent.
    pub fn set_file_tree(&mut self, files: Vec<String>) {
        self.file_tree = files;
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
            .find(|m| matches!(m.role, sc_model::Role::User))
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
             • chat — GENERIC conversation NOT about this specific project: greetings and small \
             talk (\"hello\", \"how are you\", \"thanks\"), or general questions about coding / \
             the tool itself (\"what can you do?\", \"what is Rust?\"). Use this when answering \
             needs NO knowledge of this project's files.\n\
             • question — a question ABOUT THIS project: its plan, code, or the open file (\"what \
             does this file do?\", \"what's the architecture?\", \"anything you'd change?\"). \
             Reviewing or critiquing a file is a question UNLESS they ask to write the result \
             down. Use this only when answering needs this project's context.\n\
             • todo_edit — add/remove/reorder items in the whole-project TODO backlog.\n\
             • readme_edit — change the project overview/architecture in the README.\n\
             • feature_plan — design/spec a SPECIFIC feature the user names (\"make a plan to add \
             gunner seats\", \"plan out feature X\", \"write up how we'd fix this file\"). The \
             feature is given IN the message; it does NOT come from the backlog. NOT for merely \
             running/launching something.\n\
             • plan_from_todo — plan/spec something taken FROM the TODO backlog, where the message \
             refers to the backlog rather than naming the feature (\"plan the next TODO item\", \
             \"what should we build next?\", \"pick something off the backlog and plan it\", \
             \"plan the top todo\"). Use this ONLY when the request points at the backlog for what \
             to plan.\n\
             • code_change — asking to actually edit source code (\"rename X\", \"fix this \
             function\", \"change the code\").\n\
             • command — asking to RUN / LAUNCH / BUILD / START / TEST something, i.e. execute a \
             shell command (\"start the windows client\", \"run the app\", \"build it\", \"cargo \
             test\", \"launch the server\"). Choose this over feature_plan whenever the user wants \
             something EXECUTED, not designed.{open}"
        );
        let messages = vec![
            Message::system(sys),
            Message::user(format!("Message to classify:\n{last_user}\n\nIntent:")),
        ];
        let mut req = GenerateRequest::new(messages);
        req.temperature = 0.0;
        req.max_tokens = 8;
        req.constraint = Some(sc_model::OutputConstraint::Grammar(intent_grammar()));
        req
    }

    /// Build the generate call for a classified `intent`. The system prompt is tailored to the
    /// single known intent (no four-case disambiguation for the model to get wrong), and
    /// file-producing intents attach a GBNF grammar that FORCES the output into the right
    /// `file:<name>` block — so the model structurally cannot forget the fence or pick the wrong
    /// target file. `think` controls the reasoning budget as before.
    pub fn request(&self, think: bool, intent: ChatIntent) -> GenerateRequest {
        // GENERIC chat gets a MINIMAL prompt — no README/TODO/open-file, no planning boilerplate.
        // A plain "hello" must not drag the whole project context along (the whole point of the
        // classify-first split). Everything else builds the context-bearing planning prompt,
        // trimmed to what the intent actually needs by `system_prompt`.
        let mut sys = if intent == ChatIntent::Chat {
            "You are a friendly, concise assistant inside a desktop coding app. Answer the \
             user's message directly in a sentence or two. This message isn't about their \
             project's code, so don't invent project details or propose file changes.\n\n"
                .to_string()
        } else {
            let mut s = self.system_prompt(intent);
            s.push_str(&intent_instruction(intent, self.plan_slug()));
            s
        };
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
    /// file" lands as `specs/solar-panel-tracker/spec.md`. Falls back to `feature` when no file is open.
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

    /// The planning system prompt — the design lever. Sets the posture and forbids writing
    /// code, then injects ONLY the context the classified `intent` actually needs: the
    /// file-block format for file-producing intents, the README for README/plan work, the TODO
    /// for TODO/plan work, and the open file for questions/code/plan. A `Question` about the
    /// project therefore no longer drags the whole README + TODO along — just the open file.
    /// (Generic [`ChatIntent::Chat`] never reaches here; `request` gives it a minimal prompt.)
    fn system_prompt(&self, intent: ChatIntent) -> String {
        use ChatIntent::*;
        let produces_file = matches!(intent, TodoEdit | ReadmeEdit | FeaturePlan | PlanFromTodo);
        let wants_readme = matches!(intent, ReadmeEdit | FeaturePlan | PlanFromTodo);
        // The TODO (backlog) is injected ONLY when the request is ABOUT the backlog: a TODO edit,
        // or a plan explicitly derived from it (PlanFromTodo). A plain feature spec (FeaturePlan)
        // does NOT get the whole backlog dragged into context just because it's open on screen.
        let wants_todo = matches!(intent, TodoEdit | PlanFromTodo);
        // A plan needs to name REAL files. The tree (paths only) is cheap grounding — the fix
        // for hallucinated "Files to touch" paths — without the cost of dumping file contents.
        // The spec (FeaturePlan) is WHAT/WHY only — it names no files, so it needs no file tree.
        // (The architecture step is where real file paths get resolved.) Keeping it off also
        // shrinks the prompt for the small model.
        let wants_file_tree = false;
        // A feature plan is about the PROJECT (README/TODO give the shape); the full open-file
        // dump was the bulk of a bloated ~49k-char prompt that buried the fence instruction and
        // eats a 32k-context small model's window. Questions/code changes still get the file.
        let wants_open_file = matches!(intent, Question | CodeChange);

        let mut s = String::new();
        s.push_str(
            "You are a planning partner inside a desktop coding app. You and the user shape a \
             project's PLAN together — its README (what it is / architecture) and its TODO \
             (the backlog). Be concise and fast: short, direct replies, one question at a time, \
             no walls of text. Do NOT write source code here — this is planning, not \
             implementation; the user runs a separate build step for code.\n\n",
        );
        // The NEW-vs-EXISTING framing only matters when we're actually working the plan files.
        if produces_file {
            match self.mode {
                Mode::Scratch => s.push_str(
                    "This is a NEW, empty project. Help draft a README and a TODO; propose them \
                     as files (see below).\n\n",
                ),
                Mode::Existing => s.push_str(
                    "This is an EXISTING project; its current plan files are below. Propose \
                     updated files only when the plan actually changes.\n\n",
                ),
            }
            // The shared file-block format — only needed by intents that emit one.
            s.push_str(
                "When you propose a plan file, output its FULL new contents in a fenced block \
                 whose info string is `file:<name>`, e.g.\n\
                 ```file:TODO.md\n- [ ] first task\n```\n\
                 Always put a one-line prose lead-in BEFORE any file block. You cannot edit \
                 source code here — only the plan (README/TODO/PLAN docs).\n\n",
            );
        }
        // Channel any reasoning into <think> tags so the user sees only the conclusion.
        s.push_str(
            "If you need to reason, put it inside <think>…</think> tags FIRST, then give your \
             short answer AFTER the closing tag. Never let raw reasoning be the visible reply.\n\n",
        );
        if wants_readme && !self.readme.trim().is_empty() {
            s.push_str("=== current README.md ===\n");
            s.push_str(self.readme.trim());
            s.push_str("\n\n");
        }
        if wants_todo && !self.todo.trim().is_empty() {
            s.push_str("=== current TODO.md ===\n");
            s.push_str(self.todo.trim());
            s.push_str("\n\n");
        }
        // The real project file paths, so a plan's "Files to touch" names files that EXIST.
        // Capped to keep a big repo from blowing a small model's window; paths are cheap.
        if wants_file_tree && !self.file_tree.is_empty() {
            s.push_str(
                "=== project files (real paths — 'Files to touch' MUST use paths from this list, \
                 do NOT invent paths) ===\n",
            );
            for path in self.file_tree.iter().take(FILE_TREE_MAX) {
                s.push_str(path);
                s.push('\n');
            }
            if self.file_tree.len() > FILE_TREE_MAX {
                s.push_str(&format!(
                    "… (+{} more files not shown)\n",
                    self.file_tree.len() - FILE_TREE_MAX
                ));
            }
            s.push('\n');
        }
        // The file the user is currently looking at, head-clipped — for questions/code/plan.
        if wants_open_file {
            if let Some((name, body)) = &self.open_file {
                if !body.trim().is_empty() {
                    let (clipped, cut) = clip_lines(body, OPEN_FILE_MAX_LINES);
                    s.push_str(&format!(
                        "=== file open in the code view: {name} ===\n\
                         (This is what the user is looking at. Questions like \"what does this \
                         do?\" or \"how would I change this?\" refer to this file.)\n",
                    ));
                    s.push_str(clipped.trim_end());
                    if cut {
                        s.push_str("\n… (file truncated — only the first portion is shown)");
                    }
                    s.push_str("\n\n");
                }
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
        // Generic chat is handled with a minimal prompt in `request` and never reaches here;
        // this arm is a defensive no-op so the match stays exhaustive.
        ChatIntent::Chat => String::new(),
        ChatIntent::Question => "INTENT: the user asked a QUESTION or wants a review/discussion. \
             Answer in PLAIN PROSE only. Do NOT output any file block."
            .to_string(),
        ChatIntent::TodoEdit => "INTENT: update the whole-project backlog. Output the FULL new \
             contents of `TODO.md` in a ```file:TODO.md block, after a one-line prose lead-in."
            .to_string(),
        ChatIntent::ReadmeEdit => "INTENT: update the project overview. Output the FULL new \
             contents of `README.md` in a ```file:README.md block, after a one-line prose lead-in."
            .to_string(),
        // A fresh feature spec and a backlog-derived spec produce the SAME OpenSpec doc; they
        // differ only in whether the TODO is in context (decided in `system_prompt`).
        ChatIntent::FeaturePlan | ChatIntent::PlanFromTodo => format!(
            "INTENT: write a SPEC for the feature (OpenSpec format) — WHAT it must do and WHY, \
             NOT how. Output it as a ```file:specs/{slug}/spec.md block, after a one-line prose lead-in \
             (e.g. \"Here's the spec:\"). Structure:\n\
             `# <Feature> Specification`\n\
             `## Purpose` — 1–2 sentences: what this feature delivers and why.\n\
             `## Requirements` — a bullet per requirement, each a `SHALL` statement of an \
             observable capability (e.g. \"The system SHALL let a player assign a crew member to \
             a gunner seat\").\n\
             `## Scenarios` — for the key requirements, a Given/When/Then example (Given <state>, \
             When <action>, Then <observable result>).\n\
             Describe only WHAT and WHY. Do NOT name files, modules, functions, a build order, or \
             any implementation detail — the architecture step decides how. Do NOT write source \
             code. Do NOT touch TODO.md or README.md."
        ),
        ChatIntent::CodeChange => "INTENT: the user asked to change SOURCE CODE, which you cannot \
             do from this chat. Reply in PROSE telling them to select the lines in the code view \
             on the right and comment on them. Do NOT edit TODO.md or README.md."
            .to_string(),
        ChatIntent::Command => "INTENT: the user wants to RUN something. Reply with a one-line \
             prose lead-in, then the exact shell command to run in a ```command block (a fenced \
             block whose info string is `command`), e.g.\n\
             ```command\ncargo run -p sc-win\n```\n\
             Output ONE command line only. Infer it from the project (a Rust crate → `cargo run \
             -p <crate>` / `cargo build` / `cargo test`; a script → the run command). It will run \
             in the integrated terminal. Do NOT output a file block, and do NOT write source code."
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

/// The workspace-relative path a feature spec is saved at: `specs/<slug>/spec.md` — the spec
/// lives in its own OpenSpec-style directory, so the design phases (architecture.md, layout.md,
/// breakdown.md) can sit beside it when the plan is executed.
pub fn spec_path(slug: &str) -> String {
    format!("specs/{slug}/spec.md")
}

/// Whether `path` is a feature spec: `specs/<slug>/spec.md` (the current layout), a bare
/// `specs/<name>.md` (the interim layout), or a legacy `PLAN-<slug>.md` — so existing specs and
/// plans in a project still get the Execute-plan / grounding treatment.
pub fn is_spec_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    (lower.starts_with("specs/") && lower.ends_with(".md"))
        || {
            // Legacy PLAN-<slug>.md anywhere (back-compat with existing projects).
            let name = p.rsplit('/').next().unwrap_or(&p);
            let un = name.to_ascii_uppercase();
            un.starts_with("PLAN-") && un.ends_with(".MD")
        }
}

/// Prepend a `## Request` block quoting the user's verbatim `request` to a spec's `content`,
/// so every saved spec records exactly what was asked for (provenance / traceability). Injected
/// by the app (not the model) so it's the user's exact words, not a paraphrase. No-op if the
/// content already opens with a Request block (idempotent) or the request is blank.
pub fn prepend_request(content: &str, request: &str) -> String {
    let req = request.trim();
    if req.is_empty() || content.trim_start().starts_with("## Request") {
        return content.to_string();
    }
    // Quote each line of the request as a markdown blockquote.
    let quoted: String = req
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("## Request\n{quoted}\n\n{}", content.trim_start())
}

/// Wrap a bare-prose feature plan into a `PLAN-<slug>.md` [`ProposedFile`]. Used when the
/// model returned a plan as plain prose instead of the requested ```file: block (common on
/// small local models when the prompt is large) — so a plan ALWAYS yields an Apply/verify card
/// rather than silently staying prose in the chat.
///
/// The spec is saved as `specs/<slug>.md`, named after the spec's OWN `# <Feature>
/// Specification` heading when it has one (e.g. → `specs/add-alternate-seat-types.md`) — a far
/// better name than the user's raw phrasing — falling back to `fallback_slug` (derived from the
/// request) only when the spec has no title heading.
pub fn wrap_plan_prose(prose: &str, fallback_slug: &str) -> ProposedFile {
    let slug = plan_title(prose)
        .map(|t| slugify(&t))
        .filter(|s| s != "feature")
        .unwrap_or_else(|| fallback_slug.to_string());
    ProposedFile {
        name: spec_path(&slug),
        content: prose.trim().to_string(),
    }
}

/// The title from a `## Plan: <title>` heading in `prose`, if present (case-insensitive on the
/// `Plan:` label). Used to name a wrapped plan file after its own subject.
fn plan_title(prose: &str) -> Option<String> {
    for line in prose.lines() {
        let t = line.trim_start_matches('#').trim();
        // New spec heading: `<Feature> Specification` → the feature name is the title.
        if let Some(name) = t
            .strip_suffix(" Specification")
            .or_else(|| t.strip_suffix(" specification"))
        {
            if !name.trim().is_empty() {
                return Some(name.trim().to_string());
            }
        }
        // Back-compat: the old `Plan: <title>` heading.
        if let Some(rest) = t
            .strip_prefix("Plan:")
            .or_else(|| t.strip_prefix("plan:"))
            .or_else(|| t.strip_prefix("PLAN:"))
        {
            let title = rest.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// Public slugifier so the app can name a wrapped plan after the FEATURE (from the user's
/// request) rather than the open file. E.g. "add alternate seat types" → "add-alternate-seat".
pub fn slug_for(text: &str) -> String {
    slugify(text)
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
        c.user_turn("plan the next backlog item");
        // PlanFromTodo is the context-heavy intent that injects both plan files (README + TODO).
        let req = c.request(false, ChatIntent::PlanFromTodo);
        assert_eq!(req.messages[0].role, sc_model::Role::System);
        let sys = &req.messages[0].content;
        assert!(sys.contains("My Game"), "README injected: {sys}");
        assert!(sys.contains("add lakes"), "TODO injected: {sys}");
        assert!(sys.to_lowercase().contains("plan"), "planning posture set");
        // The user turn is present after the system message.
        assert!(req
            .messages
            .iter()
            .any(|m| m.role == sc_model::Role::User && m.content.contains("next backlog item")));
    }

    #[test]
    fn classify_request_is_grammar_constrained_to_the_intent_tokens() {
        // The classifier must FORCE one intent word via GBNF — no free-text to misparse.
        let mut c = Conversation::open("# X", "- a");
        c.user_turn("can you make a plan to investigate these issues?");
        let req = c.classify_request();
        match req.constraint {
            Some(sc_model::OutputConstraint::Grammar(g)) => {
                assert!(g.contains("feature_plan"), "grammar lists intents: {g}");
                assert!(g.contains("question"), "grammar lists intents: {g}");
            }
            other => panic!("expected a grammar constraint, got {other:?}"),
        }
    }

    #[test]
    fn feature_plan_intent_targets_a_plan_file_named_after_the_open_file() {
        // A feature-plan for an open `SolarPanelTracker.cs` → specs/solar-panel-tracker/spec.md,
        // NOT a TODO edit (the reported bug).
        let mut c = Conversation::open("# X", "- a");
        c.set_open_file(Some(("Assets/Scripts/SolarPanelTracker.cs".into(), "class X {}".into())));
        c.user_turn("make a plan to investigate these");
        let sys = c.request(false, ChatIntent::FeaturePlan).messages[0]
            .content
            .clone();
        assert!(sys.contains("specs/solar-panel-tracker/spec.md"), "slug from open file: {sys}");
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
    fn generic_chat_prompt_carries_no_project_context() {
        // "hello" (classified Chat) must NOT drag the README/TODO/open-file or planning
        // boilerplate along — the whole point of the generic/coding split.
        let mut c = Conversation::open("# void_engine\nMMO space game", "- [ ] add lakes");
        c.set_open_file(Some(("main.rs".into(), "fn main() { huge_file(); }".into())));
        c.user_turn("hello");
        let sys = c.request(false, ChatIntent::Chat).messages[0].content.clone();
        assert!(!sys.contains("void_engine"), "no README injected: {sys}");
        assert!(!sys.contains("add lakes"), "no TODO injected: {sys}");
        assert!(!sys.contains("main.rs"), "no open file injected: {sys}");
        assert!(!sys.contains("file:<name>"), "no file-block boilerplate: {sys}");
        assert!(sys.len() < 500, "generic prompt stays small ({} chars)", sys.len());
    }

    #[test]
    fn question_prompt_includes_open_file_but_not_readme_or_todo() {
        // A project QUESTION gets the open file (so "what does this do?" works) but no longer
        // the whole README + TODO dump.
        let mut c = Conversation::open("# void_engine\nMMO", "- [ ] add lakes");
        c.set_open_file(Some(("main.rs".into(), "fn main() {}".into())));
        c.user_turn("what does this file do?");
        let sys = c.request(false, ChatIntent::Question).messages[0].content.clone();
        assert!(sys.contains("main.rs"), "open file injected for a question: {sys}");
        assert!(!sys.contains("add lakes"), "no TODO for a plain question: {sys}");
        assert!(!sys.contains("void_engine"), "no README for a plain question: {sys}");
    }

    #[test]
    fn feature_plan_prompt_gets_readme_but_not_the_open_file_dump() {
        // A fresh feature spec gets the README (project context) but NOT the full open-file dump
        // (which bloated the prompt and buried the fence instruction on small models). The TODO
        // exclusion is covered by `feature_plan_does_not_inject_the_todo`.
        let mut c = Conversation::open("# void_engine", "- [ ] add lakes");
        c.set_open_file(Some(("main.rs".into(), "fn giant_file() {}".into())));
        c.user_turn("plan out adding lakes");
        let sys = c.request(false, ChatIntent::FeaturePlan).messages[0].content.clone();
        assert!(sys.contains("void_engine"), "README present: {sys}");
        assert!(!sys.contains("giant_file"), "open file NOT dumped into a plan: {sys}");
    }

    #[test]
    fn feature_plan_does_not_inject_the_todo() {
        // A fresh feature spec must NOT drag the backlog into context just because it's open.
        let mut c = Conversation::open("# proj", "- [ ] add lakes\n- [ ] add rivers");
        c.user_turn("plan gunner and miner seats");
        let sys = c.request(false, ChatIntent::FeaturePlan).messages[0].content.clone();
        assert!(!sys.contains("add lakes"), "no TODO for a fresh feature spec: {sys}");
        assert!(sys.contains("proj"), "but README stays for project context");
    }

    #[test]
    fn plan_from_todo_injects_the_todo() {
        // A backlog-derived plan DOES get the TODO.
        let mut c = Conversation::open("# proj", "- [ ] add lakes\n- [ ] add rivers");
        c.user_turn("plan the next todo item");
        let sys = c.request(false, ChatIntent::PlanFromTodo).messages[0].content.clone();
        assert!(sys.contains("add lakes"), "PlanFromTodo gets the backlog: {sys}");
        // Same spec instruction as FeaturePlan.
        assert!(sys.to_lowercase().contains("shall"), "still an OpenSpec spec");
    }

    #[test]
    fn classifier_offers_plan_from_todo() {
        assert!(intent_grammar().contains("\"plan_from_todo\""), "{}", intent_grammar());
        assert_eq!(ChatIntent::parse("plan_from_todo"), ChatIntent::PlanFromTodo);
    }

    #[test]
    fn feature_plan_is_a_files_free_spec() {
        // The plan is now a SPEC (what/why only) — no file tree injected, and it must instruct
        // the model NOT to name files (the architecture step decides how).
        let mut c = Conversation::open("# void_engine", "- [ ] x");
        c.set_file_tree(vec![
            "crates/sc-core/src/agent/mod.rs".into(),
            "crates/sc-win/src/app.rs".into(),
        ]);
        c.user_turn("plan out adding seats");
        let plan = c.request(false, ChatIntent::FeaturePlan).messages[0].content.clone();
        assert!(!plan.contains("crates/sc-core"), "spec must NOT get the file tree: {plan}");
        let low = plan.to_lowercase();
        assert!(low.contains("openspec") || low.contains("shall"), "spec/openspec format: {plan}");
        assert!(low.contains("do not name files"), "instructed not to name files: {plan}");
    }

    #[test]
    fn prepend_request_quotes_the_user_message_at_the_top() {
        let out = prepend_request("# Seats Specification\n## Purpose\n...", "add gunner seats");
        assert!(out.starts_with("## Request\n> add gunner seats\n"), "{out}");
        assert!(out.contains("# Seats Specification"), "spec body preserved");
    }

    #[test]
    fn prepend_request_is_idempotent_and_skips_blank() {
        let with = prepend_request("# Spec", "do X");
        assert_eq!(prepend_request(&with, "do X"), with, "not double-prepended");
        assert_eq!(prepend_request("# Spec", "   "), "# Spec", "blank request is a no-op");
    }

    #[test]
    fn prepend_request_quotes_multiline() {
        let out = prepend_request("# Spec", "add seats\nfor gunners");
        assert!(out.contains("> add seats\n> for gunners"), "{out}");
    }

    #[test]
    fn wrap_prose_names_from_the_openspec_heading() {
        // A spec starts `# <Feature> Specification` → the wrapped file is named after <Feature>.
        let pf = wrap_plan_prose(
            "Here's the spec:\n# Alternate Seat Types Specification\n## Purpose\nAdd roles.",
            "fallback",
        );
        assert_eq!(pf.name, "specs/alternate-seat-types/spec.md");
    }

    #[test]
    fn wrap_plan_prose_names_from_the_plan_title() {
        // Prefer the plan's own `## Plan: <title>` heading over the fallback slug.
        let pf = wrap_plan_prose(
            "Here's a plan:\n## Plan: Add Alternate Seat Types\n**Approach:** add roles.",
            "can-you-make-a",
        );
        assert_eq!(pf.name, "specs/add-alternate-seat-types/spec.md");
        assert!(pf.content.contains("## Plan: Add Alternate Seat Types"));
    }

    #[test]
    fn wrap_plan_prose_falls_back_when_no_title() {
        let pf = wrap_plan_prose("just some prose with no heading", "add-lakes");
        assert_eq!(pf.name, "specs/add-lakes/spec.md");
    }

    #[test]
    fn classifier_offers_the_chat_token() {
        assert!(intent_grammar().contains("\"chat\""), "{}", intent_grammar());
        assert_eq!(ChatIntent::parse("chat"), ChatIntent::Chat);
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
    fn command_intent_asks_for_a_command_block() {
        let sys = Conversation::open("# X", "- a")
            .request(false, ChatIntent::Command)
            .messages[0]
            .content
            .to_lowercase();
        assert!(sys.contains("```command"), "command intent → command block: {sys}");
        assert!(sys.contains("integrated terminal"), "mentions the terminal: {sys}");
        // The per-intent instruction explicitly forbids a file block for a command.
        assert!(
            sys.contains("do not output a file block"),
            "command instruction forbids a file block: {sys}"
        );
    }

    #[test]
    fn extract_command_pulls_the_command_and_parse_reply_hides_it() {
        let reply = "I'll start the client:\n```command\ncargo run -p sc-win\n```\nIt'll open a window.";
        assert_eq!(extract_command(reply).as_deref(), Some("cargo run -p sc-win"));
        let (prose, files) = parse_reply(reply);
        assert!(files.is_empty(), "a command is not a file");
        assert!(prose.contains("start the client"), "lead-in kept");
        assert!(prose.contains("open a window"), "trailing prose kept");
        assert!(!prose.contains("cargo run"), "command line not left in prose: {prose:?}");
    }

    #[test]
    fn extract_command_none_when_no_block() {
        assert_eq!(extract_command("just some prose, no command"), None);
        // A plain (non-command) fence is left alone by the extractor.
        assert_eq!(extract_command("```\ncargo run\n```"), None);
    }

    #[test]
    fn classifier_offers_the_command_token() {
        // The grammar must include `command` so the model can pick it.
        assert!(intent_grammar().contains("\"command\""), "{}", intent_grammar());
        assert_eq!(ChatIntent::parse("command"), ChatIntent::Command);
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
