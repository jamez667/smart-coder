//! [`Conversation`] — the transcript, the context it injects, and the two requests it
//! builds (classify, then generate).
//!
//! The system prompt is the design lever here: it sets the planning posture, forbids
//! writing code, and injects ONLY the context the classified intent actually needs, so
//! a plain question doesn't drag the whole README + TODO + file tree into a small
//! model's window.

use sc_model::{GenerateRequest, Message};

use super::intent::{classifier_prompt, intent_instruction, ChatIntent};
use super::spec::slugify;

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

/// How many most-recent user/assistant turns to keep verbatim in the request. The system
/// prompt (with the current plan files) is always kept; older chatter is dropped so a small
/// model's window never overflows. The plan lives on disk, so dropping old turns is safe —
/// the files carry the state, not the transcript.
pub(super) const KEEP_TURNS: usize = 12;

/// Max file paths to inject into a feature-plan prompt. A big repo could have thousands of
/// files; capping keeps a small model's window safe while still grounding "Files to touch" for
/// the vast majority of projects. Paths are ~40 chars, so 400 ≈ 16k chars — comfortably within
/// budget alongside README/TODO.
const FILE_TREE_MAX: usize = 400;

/// How much of an open file to inject into the system prompt. A small model's window is
/// tight (the plan lives on disk for the same reason), so a long file is head-clipped: the top
/// of a source file (imports, types, signatures) is what a question usually needs, and the note
/// tells the model the rest was cut.
const OPEN_FILE_MAX_LINES: usize = 200;

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

    /// Build the fast classification call: given the conversation so far, ask the model which of
    /// the [`ChatIntent`] cases the latest user turn is. The reply is constrained by a GBNF
    /// grammar to exactly ONE intent token, so the result needs no parsing heuristics — the model
    /// The most recent user message, verbatim.
    ///
    /// The investigate path needs the question as the user asked it, not the assembled
    /// prompt around it: it feeds a tool-using agent loop whose task IS the question.
    pub fn last_user_message(&self) -> &str {
        self.turns
            .iter()
            .rev()
            .find(|m| matches!(m.role, sc_model::Role::User))
            .map(|m| m.content.as_str())
            .unwrap_or("")
    }

    /// How much of a previous turn is worth carrying into an investigation.
    ///
    /// A previous ANSWER is the expensive one: investigate answers are prose naming
    /// files, lines and a proposed fix, and they run to thousands of characters. The
    /// referent of "that fix" is almost always in the opening sentences, and the task
    /// anchor is already carrying an 800-entry file map against a measured 12288-token
    /// reserve — so this takes the head and stops.
    const CONTEXT_CLIP: usize = 600;

    /// The question as the user asked it, made **self-contained**.
    ///
    /// The investigate path's task anchor is one string, not a message list, so it
    /// cannot simply be handed the transcript the way [`Conversation::request`] is.
    /// For the FIRST question that costs nothing: the last user message and the
    /// question are the same thing.
    ///
    /// They stop being the same thing the moment someone writes "can you plan out that
    /// fix". Observed live: the harness passed that sentence alone into a fresh agent
    /// loop, which correctly reported it had "no record of what that fix is" and then
    /// read an unrelated file while flailing for one. The routing was right — the
    /// intent classifier sees the whole conversation and correctly called it a code
    /// question — and then the destination threw away the context that made the routing
    /// correct.
    ///
    /// So the anchor stays ONE POINTED QUESTION and gains a bounded preamble naming
    /// what came immediately before it. Not the transcript: the previous exchange only,
    /// head-clipped, and omitted entirely when there is nothing to refer back to.
    pub fn investigation_question(&self) -> String {
        let question = self.last_user_message();
        if question.is_empty() {
            return String::new();
        }
        // The exchange BEFORE this question: the previous user turn and the answer it
        // drew. Anything older is unlikely to be what a pronoun points at, and every
        // line costs anchor budget.
        let mut prior: Vec<&Message> = self
            .turns
            .iter()
            .rev()
            .skip_while(|m| !matches!(m.role, sc_model::Role::User))
            .skip(1) // the question itself
            .take(2) // the answer before it, and the question that drew it
            .collect();
        prior.reverse();
        if prior.is_empty() {
            return question.to_string();
        }

        let mut out = String::from("Earlier in this conversation:\n\n");
        for m in prior {
            let who = match m.role {
                sc_model::Role::User => "The user asked",
                _ => "You answered",
            };
            let body = clip(m.content.trim(), Self::CONTEXT_CLIP);
            out.push_str(&format!("{who}: {body}\n\n"));
        }
        out.push_str(&format!(
            "Now answer this, resolving any reference it makes to the above:\n\n{question}"
        ));
        out
    }

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
        let messages = vec![
            Message::system(classifier_prompt(&open)),
            Message::user(format!("Message to classify:\n{last_user}\n\nIntent:")),
        ];
        let mut req = GenerateRequest::new(messages);
        req.temperature = 0.0;
        // ROOM TO THINK, and NO GRAMMAR.
        //
        // This was `max_tokens = 8` plus a GBNF grammar allowing only the eight intent
        // words -- unforgeable by construction, and wrong in practice. A reasoning model
        // has not reached a conclusion after eight tokens, so the grammar forced out a
        // valid word chosen essentially at random: measured against tiel-coder-35b,
        // EVERY launch phrasing ("run the game", "launch the game", "start the game")
        // classified as todo_edit or readme_edit, which is why asking the app to launch
        // the game produced a paragraph about the README instead of a Run button.
        //
        // Unconstrained with a real budget, the same model got 6/6 right. So the call
        // now lets it reason and `ChatIntent::parse` reads the conclusion -- the LAST
        // intent word mentioned, since a model narrating its way to an answer names the
        // options it is ruling out first.
        req.max_tokens = 400;
        req
    }

    /// Build the generate call for a classified `intent`. The system prompt is tailored to the
    /// single known intent (no four-case disambiguation for the model to get wrong), and
    /// file-producing intents attach a GBNF grammar that FORCES the output into the right
    /// `file:<name>` block — so the model structurally cannot forget the fence or pick the wrong
    /// target file.
    ///
    /// `think` chooses the reasoning mode. This 8B doesn't reliably self-tag its reasoning, so:
    ///  • `think = false` (the fast default) appends `/no_think` — the model answers with the
    ///    conclusion directly, no rambling, small token budget.
    ///  • `think = true` lets it reason (`/think`) with a larger budget so it can finish; the
    ///    app hides any `<think>` block from the chat bubble.
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
        // Room for a REASONING model to think AND then answer.
        //
        // These used to be 1200/2400, sized for a model that answers directly. A reasoning
        // model does not: Tiel spends its budget in `reasoning_content` first, and measured
        // on the prompt below it burned 1663 reasoning tokens before emitting a single
        // content token. At 1200 the reply came back `finish_reason: "length"` with content
        // EMPTY — the user saw raw thinking and no answer, and it read as the model being
        // stupid rather than as us cutting it off mid-thought.
        //
        // `/no_think` does NOT save us here: Tiel ignores the directive and reasons anyway,
        // so the fast path needs a real budget too. This is a local model on the user's own
        // GPU — an unspent ceiling costs nothing, because generation stops at the stop token
        // whatever the cap says. Only a cap that is too SMALL has a price, and that price is
        // a truncated, useless answer.
        req.max_tokens = if think { 8000 } else { 4000 };
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
        // The README is where run/build instructions actually live, so a Command needs it
        // as much as a plan does. Without it the model was asked to "infer it from the
        // project" while being shown none of the project -- and a model told to produce
        // a command with nothing to read tries to go LOOK, by inventing a tool interface
        // it half-remembers from training. Observed live: a `<tool_call>` block naming
        // `subagent_type: Explore`, rendered into the chat as prose.
        let wants_readme = matches!(intent, ReadmeEdit | FeaturePlan | PlanFromTodo | Command);
        // The TODO (backlog) is injected ONLY when the request is ABOUT the backlog: a TODO edit,
        // or a plan explicitly derived from it (PlanFromTodo). A plain feature spec (FeaturePlan)
        // does NOT get the whole backlog dragged into context just because it's open on screen.
        let wants_todo = matches!(intent, TodoEdit | PlanFromTodo);
        // A plan needs to name REAL files. The tree (paths only) is cheap grounding — the fix
        // for hallucinated "Files to touch" paths — without the cost of dumping file contents.
        // The spec (FeaturePlan) is WHAT/WHY only — it names no files, so it needs no file tree.
        // (The architecture step is where real file paths get resolved.) Keeping it off also
        // shrinks the prompt for the small model.
        // Same reasoning for the tree: `cargo run -p <crate>` needs the crate's real
        // name, and the paths are cheap grounding (names only, no contents).
        let wants_file_tree = matches!(intent, Command);
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

/// Head-clip `text` to `max` characters on a word boundary, marking the cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    // Back off to the last space so the clip does not end mid-word.
    let head = match head.rsplit_once(' ') {
        Some((h, _)) if h.len() > max / 2 => h.to_string(),
        _ => head,
    };
    format!("{head} […]")
}
