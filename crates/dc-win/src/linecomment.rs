//! Line comments — the GitHub-PR-style "click a line, leave a comment" flow, and the
//! triage that routes it. A comment is classified **small** (a localized code change the
//! iterate loop can just make) or **big** (a design change that should go through planning
//! and your approval first).
//!
//! Pure/host-testable: prompt construction, verdict parsing, and instruction building live
//! here; the app owns the click UI, the backend call, and the routing to the iterate loop /
//! chat. Crucially, a "small" fix is NOT a line replacement — it's a coherent change the
//! agent makes across the file(s) (e.g. a rename updates every reference), verified by a
//! fast `cargo check`, not the whole test suite.

use dc_model::{GenerateRequest, Message};

/// A comment anchored to a line RANGE of a file, carrying the exact selected code plus a
/// small surrounding window — so the fix prompt feeds the model tight, relevant context (the
/// IDE does the scoping) instead of the whole file. Small models blow up on too much context;
/// this keeps the prompt bounded to what the change is actually about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineComment {
    /// Workspace-relative file path the comment is on.
    pub file: String,
    /// 1-based first line of the selection (inclusive).
    pub start: usize,
    /// 1-based last line of the selection (inclusive). Equals `start` for a single line.
    pub end: usize,
    /// The exact text of the selected lines (the block the comment is about).
    pub selection: String,
    /// A small window of surrounding lines (the enclosing context), numbered, so the model
    /// sees where the selection sits without the whole file. Empty if not gathered.
    pub context: String,
    /// What the user wrote.
    pub comment: String,
}

/// Number of lines of context to include on each side of the selection when assembling the
/// window (the IDE's "smart scoping" — enough to orient the model, not the whole file).
pub const CONTEXT_LINES: usize = 25;

/// Build the selection text and a numbered context window from a file's lines (each a
/// `(1-based number, text)`), for a comment spanning `start..=end`. This is where the IDE
/// does the scoping: only the selection + `CONTEXT_LINES` on each side go to the model.
pub fn scope_context(lines: &[(usize, String)], start: usize, end: usize) -> (String, String) {
    let sel: Vec<String> = lines
        .iter()
        .filter(|(n, _)| *n >= start && *n <= end)
        .map(|(_, t)| t.clone())
        .collect();
    let win_lo = start.saturating_sub(CONTEXT_LINES);
    let win_hi = end + CONTEXT_LINES;
    let ctx: Vec<String> = lines
        .iter()
        .filter(|(n, _)| *n >= win_lo && *n <= win_hi)
        .map(|(n, t)| format!("{n:>5}  {t}"))
        .collect();
    (sel.join("\n"), ctx.join("\n"))
}

/// The triage verdict: a question to answer, a small localized change, or a big one needing
/// planning. A comment that asks about the code (not requesting a change) must be ANSWERED,
/// never silently edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A question about the code ("what does this do?", "why?", "is this right?") — answer it
    /// in the chat; change nothing.
    Question,
    /// A localized code change — route to the iterate loop and just do it (coherently).
    Small,
    /// A design/architecture change — route to planning and ask for approval first.
    Big,
}

impl LineComment {
    /// A short `file:start-end` (or `file:line`) anchor label.
    fn anchor(&self) -> String {
        if self.start == self.end {
            format!("{} line {}", self.file, self.start)
        } else {
            format!("{} lines {}-{}", self.file, self.start, self.end)
        }
    }

    /// Build the fast triage request: is the comment a QUESTION (answer it), a SMALL edit, or
    /// a BIG change (plan first)? Uses `/no_think` for an instant one-word answer.
    pub fn classify_request(&self) -> GenerateRequest {
        let sys = "You triage a code-review comment on a selection. Classify it as exactly ONE \
             of:\n\
             QUESTION — the comment ASKS about the code and does NOT request a change (\"what \
             does this do?\", \"why?\", \"is this right?\", \"how does X work?\").\n\
             SMALL — a localized edit (rename, type tweak, fix a call, shorten a comment, a small \
             logic fix; still small even if it touches several references).\n\
             BIG — a new feature, an architectural/design shift, something needing a plan.\n\
             Default to QUESTION if it's not clearly asking for a change. Answer with exactly one \
             word: QUESTION, SMALL, or BIG. /no_think";
        let user = format!(
            "File: {}\nSelected:\n{}\n\nComment: {}\n\nQUESTION, SMALL, or BIG?",
            self.file,
            self.selection.trim(),
            self.comment.trim()
        );
        let mut req = GenerateRequest::new(vec![Message::system(sys), Message::user(user)]);
        req.max_tokens = 8;
        req.temperature = 0.0;
        req
    }

    /// Build a request to ANSWER a question about the selected code (streamed into the chat).
    /// The IDE feeds the selection + its context window, so the model answers from tight,
    /// relevant code — not the whole file.
    pub fn question_request(&self, think: bool) -> GenerateRequest {
        let sys = format!(
            "You answer a question about a specific piece of code, concisely. Do NOT edit \
             anything — just explain. {}",
            if think { "/think" } else { "/no_think" }
        );
        let ctx = if self.context.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nContext:\n{}", self.context)
        };
        let user = format!(
            "In `{}`, this code:\n\n{}\n\nQuestion: {}{ctx}",
            self.anchor(),
            self.selection.trim(),
            self.comment.trim(),
        );
        let mut req = GenerateRequest::new(vec![Message::system(sys), Message::user(user)]);
        req.max_tokens = if think { 1600 } else { 600 };
        req.temperature = 0.3;
        req
    }

    /// The instruction for a SMALL fix. **Feeds the model the selected code + a small context
    /// window directly** — so it does NOT read the whole file (that's what made a one-line fix
    /// slow and context-heavy). It still tells the agent to update references if the change is
    /// a rename, but starts from the exact scoped code the IDE already gathered.
    pub fn small_fix_instruction(&self) -> String {
        let ctx = if self.context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\nSurrounding context (for orientation — don't rewrite it all):\n{}\n",
                self.context
            )
        };
        format!(
            "In `{anchor}`, the reviewer selected this code:\n\n{selection}\n\n\
             and commented: \"{comment}\"\n\n\
             Make exactly this change. The selected code above is the target — you already have \
             it, so DON'T re-read the whole file. Edit it in place. If (and only if) the change \
             is a rename or otherwise affects other code, update those references too so it still \
             compiles.\n\n\
             If you ONLY changed comments or whitespace, you can finish WITHOUT run_verification \
             (comments don't affect compilation). Otherwise run_verification (a compile check) \
             and keep going until green. Make no unrelated changes.{ctx}",
            anchor = self.anchor(),
            selection = self.selection.trim(),
            comment = self.comment.trim(),
        )
    }

    /// The seed message for a BIG comment — drops the reviewer into planning with the selected
    /// code + comment as context, so the agent plans before any code changes.
    pub fn planning_seed(&self) -> String {
        format!(
            "About `{anchor}`:\n\n{selection}\n\n{comment}\n\n\
             This feels like more than a quick fix — let's plan it before changing code.",
            anchor = self.anchor(),
            selection = self.selection.trim(),
            comment = self.comment.trim(),
        )
    }
}

/// Parse the classifier's reply into a [`Verdict`]. Robust to casing/punctuation/extra words.
/// Defaults to [`Verdict::Question`] when unclear — the safest route is to *answer*, never to
/// silently edit code on an ambiguous comment.
pub fn parse_verdict(reply: &str) -> Verdict {
    let lower = reply.to_ascii_lowercase();
    // A question mark in the comment or an explicit QUESTION token → answer, don't edit.
    if lower.contains("question") {
        return Verdict::Question;
    }
    let small = lower.contains("small");
    let big = lower.contains("big");
    match (small, big) {
        (true, false) => Verdict::Small,
        (false, true) => Verdict::Big,
        // Ambiguous → treat as a question (answer, never auto-edit on uncertainty).
        _ => Verdict::Question,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(text: &str) -> LineComment {
        LineComment {
            file: "crates/city/src/sim.rs".to_string(),
            start: 42,
            end: 42,
            selection: "let pop = level * 10;".to_string(),
            context: "   41  fn compute() {\n   42  let pop = level * 10;\n   43  }".to_string(),
            comment: text.to_string(),
        }
    }

    #[test]
    fn scope_context_selects_the_range_and_windows_around_it() {
        // The IDE's scoping: selection is exactly the range; context is a window around it.
        let lines: Vec<(usize, String)> = (1..=60).map(|n| (n, format!("line {n}"))).collect();
        let (sel, ctx) = scope_context(&lines, 30, 32);
        assert_eq!(sel, "line 30\nline 31\nline 32");
        // Context includes CONTEXT_LINES on each side but not the whole file.
        assert!(ctx.contains("line 30") && ctx.contains("line 32"));
        assert!(ctx.contains("line 10"), "window reaches ~25 above: {ctx}");
        assert!(!ctx.contains("line 1\n"), "but not the whole file");
        assert!(!ctx.contains("line 60"), "and not lines far below");
    }

    #[test]
    fn classify_request_uses_the_selection_not_the_whole_file() {
        let req = comment("this variable name is bad").classify_request();
        let joined: String = req.messages.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("crates/city/src/sim.rs"));
        assert!(joined.contains("let pop = level * 10;"), "selection fed in");
        assert!(joined.contains("this variable name is bad"));
        assert!(joined.to_lowercase().contains("small") && joined.to_lowercase().contains("big"));
    }

    #[test]
    fn small_fix_instruction_feeds_selection_and_forbids_whole_file_reread() {
        let instr =
            comment("this variable name is bad, call it population").small_fix_instruction();
        assert!(
            instr.contains("let pop = level * 10;"),
            "selection fed in: {instr}"
        );
        assert!(
            instr
                .to_lowercase()
                .contains("don't re-read the whole file"),
            "must tell it NOT to slurp the whole file: {instr}"
        );
        assert!(
            instr.to_lowercase().contains("references"),
            "still updates references on a rename: {instr}"
        );
        assert!(instr.contains("run_verification"), "verifies: {instr}");
    }

    #[test]
    fn range_anchor_reads_as_a_span() {
        let mut c = comment("x");
        c.start = 10;
        c.end = 14;
        let instr = c.small_fix_instruction();
        assert!(instr.contains("lines 10-14"), "range anchor: {instr}");
    }

    #[test]
    fn planning_seed_carries_selection_and_defers_code() {
        let seed = comment("this whole growth model should be event-driven").planning_seed();
        assert!(seed.contains("crates/city/src/sim.rs"));
        assert!(seed.contains("event-driven"));
        assert!(seed.contains("let pop = level * 10;"), "selection carried");
        assert!(
            seed.to_lowercase().contains("plan"),
            "big comments go to planning first: {seed}"
        );
    }

    #[test]
    fn parse_verdict_reads_question_small_big_robustly() {
        assert_eq!(parse_verdict("QUESTION"), Verdict::Question);
        assert_eq!(parse_verdict("question.\n"), Verdict::Question);
        assert_eq!(parse_verdict("SMALL"), Verdict::Small);
        assert_eq!(parse_verdict("small.\n"), Verdict::Small);
        assert_eq!(parse_verdict("This is BIG"), Verdict::Big);
        // Ambiguous / empty → QUESTION (safest: answer, never silently edit).
        assert_eq!(parse_verdict(""), Verdict::Question);
        assert_eq!(parse_verdict("hmm"), Verdict::Question);
    }

    #[test]
    fn classify_prompt_offers_the_question_option() {
        let req = comment("what does this do?").classify_request();
        let sys = req.messages[0].content.to_lowercase();
        assert!(
            sys.contains("question"),
            "triage can classify a question: {sys}"
        );
    }

    #[test]
    fn question_request_feeds_selection_and_forbids_editing() {
        let req = comment("why is this * 10?").question_request(false);
        let joined: String = req.messages.iter().map(|m| m.content.clone()).collect();
        assert!(
            joined.contains("let pop = level * 10;"),
            "selection fed: {joined}"
        );
        assert!(
            joined.to_lowercase().contains("do not edit")
                || joined.to_lowercase().contains("just explain"),
            "must not edit — just answer: {joined}"
        );
    }
}
