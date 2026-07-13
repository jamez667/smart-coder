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

/// A comment anchored to a specific line of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineComment {
    /// Workspace-relative file path the comment is on.
    pub file: String,
    /// 1-based line number the comment anchors to.
    pub line: usize,
    /// The exact text of that line (context for the model — the anchor).
    pub line_text: String,
    /// What the user wrote.
    pub comment: String,
}

/// The triage verdict: is this a small localized change, or a big one needing planning?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A localized code change — route to the iterate loop and just do it (coherently).
    Small,
    /// A design/architecture change — route to planning and ask for approval first.
    Big,
}

impl LineComment {
    /// Build the fast classification request: given the line + comment, decide small vs. big.
    /// Uses `/no_think` for an instant one-word answer; the app parses it with [`parse_verdict`].
    pub fn classify_request(&self) -> GenerateRequest {
        let sys = "You triage a code-review comment on one line. Decide if acting on it is a \
             SMALL localized change (a rename, a type tweak, fixing a call, a small logic fix — \
             even if it touches several references, it's still small in scope) or a BIG change \
             (a new feature, an architectural/design shift, something needing a plan). Answer \
             with exactly one word: SMALL or BIG. /no_think";
        let user = format!(
            "File: {}\nLine {}: {}\n\nComment: {}\n\nSMALL or BIG?",
            self.file,
            self.line,
            self.line_text.trim(),
            self.comment.trim()
        );
        let mut req = GenerateRequest::new(vec![Message::system(sys), Message::user(user)]);
        req.max_tokens = 8;
        req.temperature = 0.0;
        req
    }

    /// The instruction for a SMALL fix — a scoped-but-coherent iterate run. It anchors the
    /// agent on the line, states the comment, and — critically — tells it to make the change
    /// *properly across the file(s)*: a rename updates every reference, not just this line.
    pub fn small_fix_instruction(&self) -> String {
        format!(
            "In the file `{file}`, regarding line {line}:\n\n    {line_text}\n\n\
             the reviewer commented: \"{comment}\"\n\n\
             Make this change properly. If it is a rename or affects other code, update ALL \
             the references and call sites — not just this one line — so the project still \
             compiles. Read what you need, edit in place, then run_verification (a compile \
             check) and keep going until it is green. Do not make unrelated changes.",
            file = self.file,
            line = self.line,
            line_text = self.line_text.trim(),
            comment = self.comment.trim(),
        )
    }

    /// The seed message for a BIG comment — a user turn that drops the reviewer into the
    /// planning chat with the line + comment as context, so the agent discusses/plans before
    /// any code changes (the user approves the plan first).
    pub fn planning_seed(&self) -> String {
        format!(
            "About `{file}` line {line} (`{line_text}`): {comment}\n\n\
             This feels like more than a quick fix — let's plan it before changing code.",
            file = self.file,
            line = self.line,
            line_text = self.line_text.trim(),
            comment = self.comment.trim(),
        )
    }
}

/// Parse the classifier's reply into a [`Verdict`]. Robust to casing/punctuation/extra words;
/// defaults to [`Verdict::Big`] when unclear (safer to over-plan than to auto-edit wrongly).
pub fn parse_verdict(reply: &str) -> Verdict {
    let lower = reply.to_ascii_lowercase();
    // Prefer an explicit token; "small" wins only if it appears and "big" doesn't dominate.
    let small = lower.contains("small");
    let big = lower.contains("big");
    match (small, big) {
        (true, false) => Verdict::Small,
        (false, true) => Verdict::Big,
        // Both or neither → default to planning (Big) as the safe route.
        _ => Verdict::Big,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(text: &str) -> LineComment {
        LineComment {
            file: "crates/city/src/sim.rs".to_string(),
            line: 42,
            line_text: "let pop = level * 10;".to_string(),
            comment: text.to_string(),
        }
    }

    #[test]
    fn classify_request_names_the_line_and_comment() {
        let req = comment("this variable name is bad").classify_request();
        let joined: String = req.messages.iter().map(|m| m.content.clone()).collect();
        assert!(joined.contains("crates/city/src/sim.rs"));
        assert!(joined.contains("let pop = level * 10;"));
        assert!(joined.contains("this variable name is bad"));
        assert!(joined.to_lowercase().contains("small") && joined.to_lowercase().contains("big"));
    }

    #[test]
    fn small_fix_instruction_demands_coherent_updates_not_one_line() {
        // The key requirement: "bad variable name" must update ALL references, not line 42 only.
        let instr =
            comment("this variable name is bad, call it population").small_fix_instruction();
        assert!(instr.contains("crates/city/src/sim.rs"), "{instr}");
        assert!(instr.contains("line 42"), "{instr}");
        assert!(
            instr.to_lowercase().contains("all the references")
                || instr.to_lowercase().contains("call sites"),
            "must tell the agent to update every reference: {instr}"
        );
        assert!(
            instr.contains("run_verification"),
            "must verify it still compiles: {instr}"
        );
    }

    #[test]
    fn planning_seed_carries_context_and_defers_code() {
        let seed = comment("this whole growth model should be event-driven").planning_seed();
        assert!(seed.contains("crates/city/src/sim.rs"));
        assert!(seed.contains("event-driven"));
        assert!(
            seed.to_lowercase().contains("plan"),
            "big comments go to planning first: {seed}"
        );
    }

    #[test]
    fn parse_verdict_reads_small_and_big_robustly() {
        assert_eq!(parse_verdict("SMALL"), Verdict::Small);
        assert_eq!(parse_verdict("small.\n"), Verdict::Small);
        assert_eq!(parse_verdict("This is BIG"), Verdict::Big);
        // Ambiguous / empty → default to Big (safe: plan rather than auto-edit).
        assert_eq!(parse_verdict(""), Verdict::Big);
        assert_eq!(parse_verdict("small or big?"), Verdict::Big);
    }
}
