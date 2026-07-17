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

use sc_model::{GenerateRequest, Message};

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

    /// Build a request for a fast LINE-ANCHORED edit: the model returns ONLY the new text for
    /// the selected lines (inside a ``` fence, which preserves leading indentation verbatim), and
    /// the IDE splices it in by line number. No `edit_file`, no whitespace/old_str matching — the
    /// thing small models fumble. Fast path for a localized change on a known range.
    pub fn replace_request(&self) -> GenerateRequest {
        let n = self.selection.lines().count().max(1);
        // The ``` fence is REQUIRED here on purpose: it preserves leading whitespace exactly.
        // Without it the model (and chat formatting) trims/normalises indentation, and the splice
        // lands mis-indented code. `extract_replacement` is hardened against broken/bare fences,
        // so keeping the fence is the robust choice — dropping it corrupted indentation.
        let sys = "You rewrite ONE specific block of code to satisfy a review comment. Rules:\n\
             - Output ONLY the new version of the SELECTED block, inside a single ``` code fence, \
             and NOTHING outside the fence (no explanation, no prose).\n\
             - Preserve each line's EXACT leading indentation (spaces) inside the fence.\n\
             - Rewrite ONLY the selected lines. Do NOT add lines from before or after the \
             selection, and do NOT append neighbouring code, comments, or functions. /no_think";
        // NOTE: no surrounding context is sent — it made small models copy neighbouring lines
        // (a comment right after the selection) into the output, GROWING the file instead of
        // editing the block. The selection alone carries its own indentation.
        let user = format!(
            "File `{}`. Rewrite EXACTLY these {n} selected line(s) — no more, no fewer concepts, \
             keeping their indentation:\n\n\
             ```\n{}\n```\n\nComment: {}\n\n\
             Return ONLY the rewritten selection inside one ``` fence. Do not include any lines \
             that were not in the selection above.",
            self.anchor(),
            self.selection.trim_end_matches('\n'),
            self.comment.trim(),
        );
        let mut req = GenerateRequest::new(vec![Message::system(sys), Message::user(user)]);
        // Enough for the block + a little; a localized edit is small.
        req.max_tokens = 900;
        req.temperature = 0.1;
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

/// Extract the replacement block from a [`replace_request`] reply: the body of the first ```
/// fence (the requested format — it preserves indentation), or the whole trimmed reply if the
/// model skipped the fence. Hardened against broken fences: an empty / fence-only reply → `None`
/// (never leak a bare ``` into the file).
///
/// [`replace_request`]: LineComment::replace_request
pub fn extract_replacement(reply: &str) -> Option<String> {
    let reply = reply.trim();
    let has_real = |s: &str| {
        s.lines().any(|l| {
            let t = l.trim();
            !t.is_empty() && t != "```"
        })
    };
    // If the reply has a code fence, the body of the FIRST fence is authoritative — even if it's
    // empty (we must NOT then fall back to the raw reply, which still contains the fence markers).
    if let Some(open) = reply.find("```") {
        let after = &reply[open + 3..];
        // Optional info string (```rust) sits on the same line; the body starts after its newline.
        let block = match after.find('\n') {
            Some(nl) => {
                let body = &after[nl + 1..];
                let end = body.find("```").unwrap_or(body.len());
                body[..end].trim_matches('\n').to_string()
            }
            None => String::new(), // fence with no newline → no body
        };
        return has_real(&block).then_some(block);
    }
    // No fence at all: the whole trimmed reply is the block, if it has real content.
    has_real(reply).then(|| reply.to_string())
}

/// Find the 1-based inclusive line range in `file_contents` that currently holds `selection`
/// (the exact block of lines the reviewer selected). Returns the *hinted* range `(start,end)`
/// when the lines there already match; otherwise searches the whole file for the block and
/// returns its true location; `None` if the block can't be found (the code changed too much).
///
/// This guards the by-line splice against any drift between the captured line numbers and the
/// file on disk — without it, a stale `start`/`end` silently writes the fix to the WRONG lines.
pub fn locate_range(
    file_contents: &str,
    start: usize,
    end: usize,
    selection: &str,
) -> Option<(usize, usize)> {
    let file: Vec<&str> = file_contents.lines().collect();
    let sel: Vec<&str> = selection.lines().collect();
    if sel.is_empty() {
        return None;
    }
    // Does the block sit exactly where we think it does? (The fast, overwhelmingly common case.)
    let matches_at = |lo0: usize| -> bool {
        lo0 + sel.len() <= file.len() && file[lo0..lo0 + sel.len()] == sel[..]
    };
    let hint_lo0 = start.saturating_sub(1);
    if end.saturating_sub(start) + 1 == sel.len() && matches_at(hint_lo0) {
        return Some((start, end));
    }
    // Drifted (or never matched): find the block's true position. Require a UNIQUE match so we
    // never splice into an identical-looking block elsewhere.
    let mut found: Option<usize> = None;
    for lo0 in 0..=file.len().saturating_sub(sel.len()) {
        if matches_at(lo0) {
            if found.is_some() {
                return None; // ambiguous — refuse rather than guess
            }
            found = Some(lo0);
        }
    }
    found.map(|lo0| (lo0 + 1, lo0 + sel.len()))
}

/// Splice `replacement` into `file_contents` in place of the 1-based line range `start..=end`
/// (inclusive). Returns the new file contents. Preserves the file's other lines exactly. The
/// replacement may be any number of lines (so a 1-line selection can become several, or fewer).
pub fn splice_lines(file_contents: &str, start: usize, end: usize, replacement: &str) -> String {
    // Work on lines, preserving a trailing newline if the file had one.
    let had_trailing_nl = file_contents.ends_with('\n');
    let lines: Vec<&str> = file_contents.lines().collect();
    let (lo, hi) = (start.saturating_sub(1), end.min(lines.len()));
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    out.extend(lines[..lo.min(lines.len())].iter().map(|s| s.to_string()));
    out.extend(replacement.lines().map(|s| s.to_string()));
    if hi < lines.len() {
        out.extend(lines[hi..].iter().map(|s| s.to_string()));
    }
    let mut joined = out.join("\n");
    if had_trailing_nl {
        joined.push('\n');
    }
    joined
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
    fn replace_request_asks_for_only_the_block() {
        let req = comment("un-indent this").replace_request();
        let joined: String = req.messages.iter().map(|m| m.content.clone()).collect();
        assert!(
            joined.to_lowercase().contains("only"),
            "asks for only the block: {joined}"
        );
        assert!(joined.contains("let pop = level * 10;"), "selection fed in");
    }

    #[test]
    fn extract_replacement_pulls_the_fence_body() {
        assert_eq!(
            extract_replacement("Sure:\n```rust\nlet pop = level * 10;\n```").as_deref(),
            Some("let pop = level * 10;")
        );
        // Multi-line block.
        assert_eq!(
            extract_replacement("```\na\nb\n```").as_deref(),
            Some("a\nb")
        );
        // No fence → the whole trimmed reply.
        assert_eq!(
            extract_replacement("  let x = 1;  ").as_deref(),
            Some("let x = 1;")
        );
        assert!(extract_replacement("").is_none());
        // A reply that is ONLY fence markers must NOT leak a bare ``` into the file.
        assert!(extract_replacement("```").is_none(), "lone fence → None");
        assert!(extract_replacement("```rust\n```").is_none(), "empty fenced block → None");
        assert!(extract_replacement("```\n```").is_none());
        // Info string on the fence line is dropped; body kept.
        assert_eq!(
            extract_replacement("```rust\n/// short\n```").as_deref(),
            Some("/// short")
        );
    }

    #[test]
    fn splice_replaces_the_exact_line_range() {
        let file = "line1\nline2\nline3\nline4\n";
        // Replace lines 2-3 with one new line.
        let out = splice_lines(file, 2, 3, "NEW");
        assert_eq!(
            out, "line1\nNEW\nline4\n",
            "range replaced, trailing nl kept"
        );
        // Replace a single line (2) with two lines.
        let out2 = splice_lines(file, 2, 2, "A\nB");
        assert_eq!(out2, "line1\nA\nB\nline3\nline4\n");
        // A file with no trailing newline stays that way.
        let out3 = splice_lines("a\nb", 1, 1, "X");
        assert_eq!(out3, "X\nb");
    }

    #[test]
    fn locate_range_matches_hint_when_correct() {
        let file = "a\nb\nc\nd\ne\n";
        // Hint 2-3 = "b\nc" and that's exactly what's there.
        assert_eq!(locate_range(file, 2, 3, "b\nc"), Some((2, 3)));
    }

    #[test]
    fn locate_range_reanchors_when_hint_drifted() {
        // The selected block "c\nd" is really at lines 3-4, but the hint wrongly says 1-2.
        let file = "a\nb\nc\nd\ne\n";
        assert_eq!(
            locate_range(file, 1, 2, "c\nd"),
            Some((3, 4)),
            "re-anchors to the block's true location, not the stale hint"
        );
    }

    #[test]
    fn locate_range_refuses_ambiguous_block() {
        // "x" appears twice → refuse rather than splice into the wrong one.
        let file = "x\ny\nx\nz\n";
        assert_eq!(locate_range(file, 1, 1, "x"), Some((1, 1)), "hint 1 matches exactly");
        // But if the hint doesn't match at 3 (say hint says 2), the search finds two "x" → None.
        assert_eq!(locate_range(file, 2, 2, "x"), None, "two matches → ambiguous → None");
    }

    #[test]
    fn locate_range_none_when_missing() {
        let file = "a\nb\nc\n";
        assert_eq!(locate_range(file, 1, 1, "zzz"), None);
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
