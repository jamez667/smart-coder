//! What the user's latest turn is asking for, and the prompt pieces that follow from it.
//!
//! The intent is decided by a fast, grammar-constrained classification call — NOT by
//! string-matching the reply. The grammar makes the label unforgeable, so
//! [`ChatIntent::parse`] never has to guess, and the generate step then gets ONE
//! unambiguous instruction instead of a four-case menu to misread.

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
    pub(super) fn token(self) -> &'static str {
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
    pub(super) fn all() -> [ChatIntent; 8] {
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

/// The GBNF grammar for the intent classifier: the whole output must be exactly one intent
/// token. This makes the classification unforgeable — the model can only emit a valid label, so
/// [`ChatIntent::parse`] never has to guess.
pub(super) fn intent_grammar() -> String {
    let alts = ChatIntent::all()
        .into_iter()
        .map(|i| format!("\"{}\"", i.token()))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("root ::= {alts}")
}

/// The system prompt for the classification call: the whole intent taxonomy, written for the
/// model rather than for a reader of this crate. `open` names the file currently in the code
/// view (when there is one) so "what does this do?" classifies as a question about the project.
pub(super) fn classifier_prompt(open: &str) -> String {
    format!(
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
    )
}

/// The per-intent generate instruction, appended to the base system prompt once the intent is
/// known. Because the intent is already decided, each instruction is unambiguous — no four-case
/// menu for the model to misread. `slug` is the plan filename slug for a feature plan.
pub(super) fn intent_instruction(intent: ChatIntent, slug: String) -> String {
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
