//! Chat engine tests: intent routing, what each intent's prompt injects, and reply parsing.

use super::conversation::{Conversation, Mode, KEEP_TURNS};
use super::intent::{intent_grammar, ChatIntent};
use super::reply::{extract_command, parse_reply, proposed_fix, suggested_command, visible_so_far};
use super::spec::{prepend_request, wrap_plan_prose};

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

/// **The classifier is deliberately UNCONSTRAINED, with room to think.**
///
/// This test previously asserted the opposite: a GBNF grammar allowing only the eight
/// intent words, with `max_tokens = 8`. The theory was that constraining the output
/// makes classification unforgeable. In practice it made it random — a grammar forces
/// a token out *now*, and a reasoning model has not concluded anything after eight
/// tokens, so it emitted a VALID word chosen arbitrarily.
///
/// Measured against tiel-coder-35b: every launch phrasing ("run the game", "launch the
/// game", "start the game") came back `todo_edit` or `readme_edit`, which is why asking
/// the app to launch the game produced prose about the README instead of a Run button.
/// Unconstrained with a real budget, the same model scored 17/17 across the taxonomy.
///
/// A grammar guarantees the SHAPE of an answer, never its correctness.
#[test]
fn classify_request_lets_the_model_reason_before_answering() {
    let mut c = Conversation::open("# X", "- a");
    c.user_turn("can you make a plan to investigate these issues?");
    let req = c.classify_request();
    assert!(
        req.constraint.is_none(),
        "the classifier must not constrain the reply: {:?}",
        req.constraint
    );
    assert!(
        req.max_tokens >= 100,
        "a reasoning model needs room to reach a conclusion, got {}",
        req.max_tokens
    );
    // Deterministic: the same message must classify the same way every time.
    assert_eq!(req.temperature, 0.0);
    // The taxonomy still reaches the model -- it is in the system prompt rather than
    // in a grammar.
    let sys = &req.messages[0].content;
    assert!(
        sys.contains("feature_plan"),
        "taxonomy in the prompt: {sys}"
    );
    assert!(sys.contains("command"), "taxonomy in the prompt: {sys}");
}

#[test]
fn feature_plan_intent_targets_a_plan_file_named_after_the_open_file() {
    // A feature-plan for an open `SolarPanelTracker.cs` → specs/solar-panel-tracker/spec.md,
    // NOT a TODO edit (the reported bug).
    let mut c = Conversation::open("# X", "- a");
    c.set_open_file(Some((
        "Assets/Scripts/SolarPanelTracker.cs".into(),
        "class X {}".into(),
    )));
    c.user_turn("make a plan to investigate these");
    let sys = c.request(false, ChatIntent::FeaturePlan).messages[0]
        .content
        .clone();
    assert!(
        sys.contains("specs/solar-panel-tracker/spec.md"),
        "slug from open file: {sys}"
    );
    assert!(
        sys.to_lowercase().contains("do not touch todo"),
        "TODO off-limits: {sys}"
    );
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
    assert!(
        Conversation::open("", "")
            .request(false, ChatIntent::FeaturePlan)
            .max_tokens
            >= 1200
    );
}

#[test]
fn open_file_is_injected_into_the_prompt_and_head_clipped() {
    let mut c = Conversation::open("# App", "- [ ] x");
    let body: String = (1..=500).map(|n| format!("line {n}\n")).collect();
    c.set_open_file(Some(("src/water.rs".to_string(), body)));
    let sys = c.request(false, ChatIntent::Question).messages[0]
        .content
        .clone();
    assert!(
        sys.contains("file open in the code view: src/water.rs"),
        "name shown"
    );
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
    assert!(
        !prose.contains("/no_think"),
        "directive stripped: {prose:?}"
    );
    assert!(
        !prose.contains("tool_call"),
        "tool marker stripped: {prose:?}"
    );
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
    c.set_open_file(Some((
        "main.rs".into(),
        "fn main() { huge_file(); }".into(),
    )));
    c.user_turn("hello");
    let sys = c.request(false, ChatIntent::Chat).messages[0]
        .content
        .clone();
    assert!(!sys.contains("void_engine"), "no README injected: {sys}");
    assert!(!sys.contains("add lakes"), "no TODO injected: {sys}");
    assert!(!sys.contains("main.rs"), "no open file injected: {sys}");
    assert!(
        !sys.contains("file:<name>"),
        "no file-block boilerplate: {sys}"
    );
    assert!(
        sys.len() < 500,
        "generic prompt stays small ({} chars)",
        sys.len()
    );
}

#[test]
fn question_prompt_includes_open_file_but_not_readme_or_todo() {
    // A project QUESTION gets the open file (so "what does this do?" works) but no longer
    // the whole README + TODO dump.
    let mut c = Conversation::open("# void_engine\nMMO", "- [ ] add lakes");
    c.set_open_file(Some(("main.rs".into(), "fn main() {}".into())));
    c.user_turn("what does this file do?");
    let sys = c.request(false, ChatIntent::Question).messages[0]
        .content
        .clone();
    assert!(
        sys.contains("main.rs"),
        "open file injected for a question: {sys}"
    );
    assert!(
        !sys.contains("add lakes"),
        "no TODO for a plain question: {sys}"
    );
    assert!(
        !sys.contains("void_engine"),
        "no README for a plain question: {sys}"
    );
}

#[test]
fn feature_plan_prompt_gets_readme_but_not_the_open_file_dump() {
    // A fresh feature spec gets the README (project context) but NOT the full open-file dump
    // (which bloated the prompt and buried the fence instruction on small models). The TODO
    // exclusion is covered by `feature_plan_does_not_inject_the_todo`.
    let mut c = Conversation::open("# void_engine", "- [ ] add lakes");
    c.set_open_file(Some(("main.rs".into(), "fn giant_file() {}".into())));
    c.user_turn("plan out adding lakes");
    let sys = c.request(false, ChatIntent::FeaturePlan).messages[0]
        .content
        .clone();
    assert!(sys.contains("void_engine"), "README present: {sys}");
    assert!(
        !sys.contains("giant_file"),
        "open file NOT dumped into a plan: {sys}"
    );
}

#[test]
fn feature_plan_does_not_inject_the_todo() {
    // A fresh feature spec must NOT drag the backlog into context just because it's open.
    let mut c = Conversation::open("# proj", "- [ ] add lakes\n- [ ] add rivers");
    c.user_turn("plan gunner and miner seats");
    let sys = c.request(false, ChatIntent::FeaturePlan).messages[0]
        .content
        .clone();
    assert!(
        !sys.contains("add lakes"),
        "no TODO for a fresh feature spec: {sys}"
    );
    assert!(sys.contains("proj"), "but README stays for project context");
}

#[test]
fn plan_from_todo_injects_the_todo() {
    // A backlog-derived plan DOES get the TODO.
    let mut c = Conversation::open("# proj", "- [ ] add lakes\n- [ ] add rivers");
    c.user_turn("plan the next todo item");
    let sys = c.request(false, ChatIntent::PlanFromTodo).messages[0]
        .content
        .clone();
    assert!(
        sys.contains("add lakes"),
        "PlanFromTodo gets the backlog: {sys}"
    );
    // Same spec instruction as FeaturePlan.
    assert!(
        sys.to_lowercase().contains("shall"),
        "still an OpenSpec spec"
    );
}

#[test]
fn classifier_offers_plan_from_todo() {
    assert!(
        intent_grammar().contains("\"plan_from_todo\""),
        "{}",
        intent_grammar()
    );
    assert_eq!(
        ChatIntent::parse("plan_from_todo"),
        ChatIntent::PlanFromTodo
    );
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
    let plan = c.request(false, ChatIntent::FeaturePlan).messages[0]
        .content
        .clone();
    assert!(
        !plan.contains("crates/sc-core"),
        "spec must NOT get the file tree: {plan}"
    );
    let low = plan.to_lowercase();
    assert!(
        low.contains("openspec") || low.contains("shall"),
        "spec/openspec format: {plan}"
    );
    assert!(
        low.contains("do not name files"),
        "instructed not to name files: {plan}"
    );
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
    assert_eq!(
        prepend_request("# Spec", "   "),
        "# Spec",
        "blank request is a no-op"
    );
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
    assert!(
        intent_grammar().contains("\"chat\""),
        "{}",
        intent_grammar()
    );
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
    assert!(
        sys.contains("do not output any file"),
        "no file block: {sys}"
    );
}

#[test]
fn todo_intent_targets_todo_and_code_change_targets_neither() {
    let todo = Conversation::open("# X", "- a")
        .request(false, ChatIntent::TodoEdit)
        .messages[0]
        .content
        .clone();
    assert!(
        todo.contains("file:TODO.md"),
        "todo edit → TODO block: {todo}"
    );
    let code = Conversation::open("# X", "- a")
        .request(false, ChatIntent::CodeChange)
        .messages[0]
        .content
        .to_lowercase();
    assert!(
        code.contains("cannot"),
        "code change refused in chat: {code}"
    );
    assert!(
        code.contains("comment"),
        "steers to code-view comment: {code}"
    );
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
    assert!(
        sys.contains("```command"),
        "command intent → command block: {sys}"
    );
    assert!(
        sys.contains("integrated terminal"),
        "mentions the terminal: {sys}"
    );
    // The per-intent instruction explicitly forbids a file block for a command.
    assert!(
        sys.contains("do not output a file block"),
        "command instruction forbids a file block: {sys}"
    );
}

#[test]
fn extract_command_pulls_the_command_and_parse_reply_hides_it() {
    let reply =
        "I'll start the client:\n```command\ncargo run -p sc-win\n```\nIt'll open a window.";
    assert_eq!(
        extract_command(reply).as_deref(),
        Some("cargo run -p sc-win")
    );
    let (prose, files) = parse_reply(reply);
    assert!(files.is_empty(), "a command is not a file");
    assert!(prose.contains("start the client"), "lead-in kept");
    assert!(prose.contains("open a window"), "trailing prose kept");
    assert!(
        !prose.contains("cargo run"),
        "command line not left in prose: {prose:?}"
    );
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
    assert!(
        intent_grammar().contains("\"command\""),
        "{}",
        intent_grammar()
    );
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

/// **A reasoning model's thinking must never be the visible reply.**
///
/// Measured against Tiel on a real prompt: the server returns thinking in a SEPARATE
/// `reasoning_content` field, not as `<think>` tags in `content`. The backend now tags
/// each streamed reasoning delta, so a reply arrives as MANY `<think>` blocks — and
/// `strip_think` used to remove only the first, leaving the rest on screen as the
/// "Wait — I'm Tiel-Coder… Actually, let me reconsider" wall the user reported.
#[test]
fn every_think_block_is_stripped_not_just_the_first() {
    let streamed = "<think>The user is asking</think><think>Wait, let me reconsider</think>The trail is drawn tail-first.";
    assert_eq!(
        visible_so_far(streamed),
        "The trail is drawn tail-first.",
        "multi-block reasoning must be fully hidden"
    );
}

/// Reasoning with NO answer yet (the truncation case) must show nothing, not the thinking.
#[test]
fn reasoning_with_no_answer_shows_nothing() {
    assert_eq!(
        visible_so_far("<think>still working</think><think>and more</think>"),
        ""
    );
    // The measured failure: budget spent entirely on reasoning, content empty.
    assert_eq!(visible_so_far("<think>burned the whole budget"), "");
}

/// The investigation anchor must survive a FOLLOW-UP question (spec 23 / the
/// investigate path).
mod investigation_question {
    use super::*;

    /// **A first question is unchanged.** The whole reason the anchor took only the
    /// last message was that a task anchor is one pointed question, not a transcript,
    /// and that reasoning is right for the opening turn. Nothing here may spend anchor
    /// budget on a preamble when there is nothing to refer back to.
    fn convo() -> Conversation {
        Conversation::open("# Readme", "- todo")
    }

    #[test]
    fn a_first_question_is_passed_through_verbatim() {
        let mut c = convo();
        c.user_turn("why is the trail behind the stars thin before it gets thick?");
        assert_eq!(
            c.investigation_question(),
            "why is the trail behind the stars thin before it gets thick?"
        );
    }

    /// **The bug this exists for.** Observed live: "Can you plan out that fix" was
    /// passed alone into a fresh agent loop, which correctly reported it had "no record
    /// of what that fix is" and then read an unrelated file while flailing. The routing
    /// was right; the destination threw away the context that made it right.
    #[test]
    fn a_follow_up_carries_what_its_pronoun_points_at() {
        let mut c = convo();
        c.user_turn("why is the trail behind the stars thin before it gets thick?");
        c.record_reply(
            "The trail is drawn as two segments in draw_trails in \
             crates/void_engine/src/fx/starfield.rs. The fix is to swap width_head and \
             width_tail on the two batch.line calls.",
        );
        c.user_turn("Can you plan out that fix");

        let q = c.investigation_question();
        // The question itself is still the thing being asked, and asked last.
        assert!(q.trim_end().ends_with("Can you plan out that fix"), "{q}");
        // And the referent travelled with it.
        assert!(q.contains("Earlier in this conversation"), "{q}");
        assert!(q.contains("draw_trails"), "{q}");
        assert!(q.contains("starfield.rs"), "{q}");
        // Both sides of the prior exchange are named, so "that fix" is resolvable.
        assert!(q.contains("The user asked"), "{q}");
        assert!(q.contains("You answered"), "{q}");
    }

    /// A previous ANSWER is the expensive turn — investigate replies run to thousands
    /// of characters — so it is head-clipped rather than pasted whole. The anchor is
    /// already carrying an 800-entry file map against a measured token reserve.
    #[test]
    fn a_long_previous_answer_is_clipped_not_pasted() {
        let mut c = convo();
        c.user_turn("why is the trail thin?");
        c.record_reply(&format!(
            "The fix is in starfield.rs. {}",
            "padding ".repeat(400)
        ));
        c.user_turn("plan that out");

        let q = c.investigation_question();
        // The head survives -- that is where the referent lives.
        assert!(q.contains("starfield.rs"), "{q}");
        // The tail does not, and the cut is marked.
        assert!(q.contains("[…]"), "{q}");
        assert!(
            q.len() < 2000,
            "anchor preamble too long: {} chars",
            q.len()
        );
    }

    #[test]
    fn an_empty_conversation_yields_an_empty_question() {
        assert!(convo().investigation_question().is_empty());
    }
}

/// Offering to make the fix (the investigate path).
mod offering_the_fix {
    use super::*;

    /// **The case from the screenshot.** An investigation named the file, the lines and
    /// the change; the user then had to ask "can you do it?" and got the fix restated a
    /// third time, because chat runs the read-only registry. The offer should come with
    /// the answer.
    #[test]
    fn an_answer_naming_a_file_and_a_fix_is_offered() {
        let answer = "The trail is drawn in crates/void_engine/src/fx/starfield.rs, in \
                      draw_trails (around line 168). The fix is to swap the widths so the \
                      head is the thick segment and the tail is the thin one.";
        let fix = proposed_fix("why is the trail thin before it gets thick?", answer)
            .expect("should offer");
        // The instruction carries the ANSWER -- that is where the file and remedy are.
        assert!(fix.contains("starfield.rs"), "{fix}");
        assert!(fix.contains("draw_trails"), "{fix}");
        // And the question, as context for why.
        assert!(fix.contains("why is the trail thin"), "{fix}");
        // Scoped: an iterate run must not wander off refactoring.
        assert!(fix.contains("nothing else"), "{fix}");
    }

    /// **An offer the user did not want is worse than no offer**, because clicking it
    /// edits their source. An explanation that proposes no change must not sprout a
    /// button.
    #[test]
    fn an_explanation_that_proposes_no_change_is_not_offered() {
        // Names a file, but only describes how it works.
        let explain = "Rendering happens in crates/void_engine/src/fx/starfield.rs. \
                       draw_trails runs once per frame and iterates the star list, \
                       drawing two line segments per star.";
        assert!(proposed_fix("how does the starfield render?", explain).is_none());
    }

    #[test]
    fn an_answer_naming_no_file_is_not_offered() {
        // Reads like a fix, but there is nothing for an iterate run to open.
        let vague = "The bug is that the widths are the wrong way round; you should swap them.";
        assert!(proposed_fix("why is it backwards?", vague).is_none());
    }

    #[test]
    fn an_empty_answer_is_not_offered() {
        assert!(proposed_fix("why?", "").is_none());
        assert!(proposed_fix("why?", "   ").is_none());
    }
}

/// Reasoning tags must never reach a chat bubble — including from the investigate
/// path's live progress lines.
mod thinking_never_leaks {
    use super::*;

    /// **The exact shape the user saw.** A reasoning model streams into
    /// `reasoning_content`, and the backend wraps EACH DELTA in its own `<think>` pair
    /// — deliberately, because that is the one representation every consumer strips.
    /// The investigate progress path was the consumer that did not, so a turn arrived
    /// as one tag pair per token and was echoed verbatim.
    #[test]
    fn per_token_think_tags_are_stripped_entirely() {
        let raw = "<think> task </think><think> is </think><think> clear </think>\
                   <think>.</think><think> I</think><think> need </think>\
                   <think> to </think><think> edit </think><think> void </think>";
        assert!(visible_so_far(raw).is_empty(), "{:?}", visible_so_far(raw));
    }

    /// The real prose in front of the tags survives — the progress line is meant to say
    /// what the model is doing, and throwing that away was the older bug this path was
    /// written to fix.
    #[test]
    fn prose_before_the_thinking_survives() {
        let raw = "Reading the trail code now.<think>let me check the widths</think>";
        assert_eq!(visible_so_far(raw), "Reading the trail code now.");
    }

    /// An unterminated block is the model running out of budget mid-thought. Everything
    /// after the opening tag is dropped rather than shown as a half-sentence of
    /// reasoning.
    #[test]
    fn an_unterminated_think_block_shows_nothing_after_it() {
        let raw = "Checking.<think>Wait, actually the head is the leading edge and";
        assert_eq!(visible_so_far(raw), "Checking.");
    }

    /// The answer itself gets the same treatment. A grammar-constrained `finish` does
    /// not normally carry tags — but "does not normally" is exactly how the progress
    /// lines leaked.
    #[test]
    fn a_finish_answer_carrying_tags_is_cleaned() {
        let arg = "<think>checking</think>The fix is in starfield.rs: swap the widths.";
        assert_eq!(
            visible_so_far(arg),
            "The fix is in starfield.rs: swap the widths."
        );
    }
}

/// Reading the classifier's answer out of a reasoning model's narration.
mod classifier_parsing {
    use super::*;

    /// **The conclusion is at the end.** A reasoning model names the options it is
    /// RULING OUT before it commits, so taking the first token mentioned returned the
    /// rejected one.
    #[test]
    fn the_last_intent_word_wins_not_the_first() {
        let reply = "This is not a todo_edit, and it is not a feature_plan -- the user                      wants something executed. command";
        assert_eq!(ChatIntent::parse(reply), ChatIntent::Command);
    }

    /// A bare word (what the model emits when it does not narrate) still parses.
    #[test]
    fn a_bare_intent_word_parses() {
        assert_eq!(ChatIntent::parse("command"), ChatIntent::Command);
        assert_eq!(
            ChatIntent::parse(
                "  question
"
            ),
            ChatIntent::Question
        );
        assert_eq!(ChatIntent::parse("CHAT"), ChatIntent::Chat);
    }

    /// Reasoning tags are stripped before parsing, so a `<think>` block naming other
    /// intents cannot outvote the conclusion after it.
    #[test]
    fn thinking_does_not_outvote_the_answer() {
        let reply = "<think>maybe todo_edit? no, readme_edit? no</think>command";
        assert_eq!(ChatIntent::parse(reply), ChatIntent::Command);
    }

    /// Nothing recognizable falls back to Question -- the safe default, since it is the
    /// intent that reads the code before answering.
    #[test]
    fn an_unrecognizable_reply_falls_back_to_question() {
        assert_eq!(ChatIntent::parse(""), ChatIntent::Question);
        assert_eq!(ChatIntent::parse("I'm not sure"), ChatIntent::Question);
    }
}

/// A command the answer ME\nTIO\nED is as runnable as one it proposed.
mod runnable_commands_in_prose {
    use super::*;

    /// **The reported case.** Asked to launch the game, the model answered correctly --
    /// it found the README's instructions and quoted the cargo line -- and there was
    /// nothing to click.
    #[test]
    fn a_command_quoted_in_prose_is_offered() {
        let reply = "You can launch the game client with:\n\n\
                     cargo run -p void_claim --release\n\n\
                     This is documented in the project README under the ## run (local dev) \
                     section.";
        assert_eq!(
            suggested_command(reply).as_deref(),
            Some("cargo run -p void_claim --release")
        );
    }

    #[test]
    fn a_shell_fenced_block_is_offered() {
        let reply = "Run the tests with:\n\n```bash\ncargo test --workspace\n```";
        assert_eq!(
            suggested_command(reply).as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn a_console_block_with_a_prompt_marker_is_stripped() {
        let reply = "Try:\n\n```console\n$ npm run dev\n```";
        assert_eq!(suggested_command(reply).as_deref(), Some("npm run dev"));
    }

    /// The explicit block still wins: the `Command` intent was ASKED for a command, and
    /// a guess must never override an instruction.
    #[test]
    fn an_explicit_command_block_takes_precedence() {
        let reply = "Do this:\n\n```command\ncargo run -p sc-win\n```\n\n\
                     (not cargo build --release, which only builds)";
        assert_eq!(
            extract_command(reply).as_deref(),
            Some("cargo run -p sc-win")
        );
        // `suggested_command` defers rather than competing.
        assert!(suggested_command(reply).is_none());
    }

    /// **A button that runs the wrong thing is worse than no button.** The cost of a
    /// miss is the user types it themselves; the cost of a false positive is an
    /// unexpected process. So these must all stay silent.
    #[test]
    fn prose_about_commands_is_not_a_command() {
        for reply in [
            // Backticked prose -- talking about a command, not giving one.
            "You would normally run `cargo test` here, but the harness does that for you.",
            // A sentence that merely starts with a tool name.
            "cargo is the Rust build tool. It handles dependencies and builds.",
            // A pipeline: too easy to get wrong, and the sandbox would likely refuse it.
            "cargo test 2>&1 | grep FAILED",
            // A chain.
            "cargo build && cargo run",
            // A redirect.
            "cargo test > out.txt",
            // \not a build tool at all.
            "rm -rf target",
            "git push origin main",
            // \nothing runnable.
            "The trail is drawn in starfield.rs, in draw_trails.",
            "",
        ] {
            assert!(
                suggested_command(reply).is_none(),
                "should \nOT offer a button for: {reply:?} (got {:?})",
                suggested_command(reply)
            );
        }
    }

    /// A language fence that is not a shell is code being shown, not run.
    #[test]
    fn a_rust_block_is_not_a_command() {
        let reply = "The fix is:\n\n```rust\nlet width_tail = width_head * 0.55;\n```";
        assert!(suggested_command(reply).is_none());
    }

    #[test]
    fn reasoning_is_stripped_before_looking() {
        let reply = "<think>maybe cargo build? no</think>Run it with:\n\ncargo run -p void_claim";
        assert_eq!(
            suggested_command(reply).as_deref(),
            Some("cargo run -p void_claim")
        );
    }
}

/// A chat with no tools must never render a tool call.
mod hallucinated_tool_calls {
    use super::*;

    /// **The reported case, verbatim.** The planning chat is a single completion with
    /// no registry and nothing to call, but a model trained on agent harnesses reaches
    /// for one anyway -- and it was rendered into the bubble as prose.
    #[test]
    fn a_hallucinated_tool_call_never_reaches_the_bubble() {
        let reply = "I need to see what is in this project to know how to launch it.

                     <function=filesystem_list_dir>
                     <parameter=path>
.
</parameter>
                     <parameter=include_hidden_dirs>
False
</parameter>
                     </function>";
        let out = visible_so_far(reply);
        assert_eq!(
            out,
            "I need to see what is in this project to know how to launch it."
        );
        assert!(!out.contains("function"), "{out}");
        assert!(!out.contains("parameter"), "{out}");
    }

    /// The other syntax the same model produced, wrapped in `<tool_call>`.
    #[test]
    fn a_tool_call_wrapper_is_stripped_with_its_contents() {
        let reply = "I will look.

<tool_call>
<function=task>
                     <parameter=subagent_type>
Explore
</parameter>
                     </function>
</tool_call>";
        assert_eq!(visible_so_far(reply), "I will look.");
    }

    /// Cut off mid-call (the model ran out of budget): everything from the opening tag
    /// is machinery, so none of it is shown.
    #[test]
    fn an_unterminated_tool_call_drops_everything_after_it() {
        let reply = "Checking the project.
<tool_call>
<function=list_dir>
<parameter=path>";
        assert_eq!(visible_so_far(reply), "Checking the project.");
    }

    /// A real answer is untouched -- the stripper must not eat prose that merely talks
    /// about functions or parameters.
    #[test]
    fn ordinary_prose_survives_unchanged() {
        let reply = "The draw_trails function takes an intensity parameter, and the fix                      is in its width calculation.";
        assert_eq!(visible_so_far(reply), reply);
    }

    /// A command block still survives, since that is the thing the Command intent is
    /// asked for.
    #[test]
    fn a_command_block_is_not_confused_for_a_tool_call() {
        let reply = "Launch it with:

```command
cargo run -p void_claim --release
```";
        assert_eq!(
            extract_command(reply).as_deref(),
            Some("cargo run -p void_claim --release")
        );
    }
}

/// History is stored CLEAN, so the model never re-reads its own thinking.
mod stored_history_is_clean {
    use super::*;

    /// **Found in a live log.** Every display path stripped tags, so the bubbles looked
    /// right while the stored history quietly filled with them -- and an investigation
    /// built its task anchor from that history, producing:
    ///
    /// ```text
    /// You answered: <think>The</think><think> user</think><think> wants</think>...
    /// ```
    #[test]
    fn a_reply_is_stored_without_its_reasoning() {
        let mut c = Conversation::open("# X", "- a");
        c.user_turn("can you start the game");
        c.record_reply(
            "<think>The</think><think> user</think><think> wants</think>             </think>The game client is the void_claim crate.",
        );
        c.user_turn("in the jump screen the star is on the wrong side");

        let anchor = c.investigation_question();
        assert!(!anchor.contains("<think>"), "tags in the anchor: {anchor}");
        assert!(!anchor.contains("</think>"), "tags in the anchor: {anchor}");
        assert!(
            anchor.contains("void_claim"),
            "the answer survived: {anchor}"
        );
    }

    /// A reply that was ENTIRELY reasoning stores nothing rather than storing tags --
    /// and the turn is kept, so user/assistant alternation is not desynchronised.
    #[test]
    fn an_all_reasoning_reply_stores_empty_but_keeps_the_turn() {
        let mut c = Conversation::open("# X", "- a");
        c.user_turn("hi");
        c.record_reply("<think>hmm</think><think>nothing to say</think>");
        c.user_turn("still there?");
        let anchor = c.investigation_question();
        assert!(!anchor.contains("think"), "{anchor}");
    }
}
