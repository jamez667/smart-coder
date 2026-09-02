//! The zoned, hard-budgeted prompt assembler (spec 05 — the budget).
//!
//! Every prompt is built to fit a hard token budget = an *effective* fraction of
//! the backend's advertised window, minus a reserve for the model's reply. The
//! budget is split into priority-ordered zones; under pressure the manager evicts
//! from the lowest priority up and **never** drops the sacred zones (task anchor,
//! current step, most recent observation).

use sc_model::Message;

use crate::tokens::TokenCounter;

/// A prompt zone, in *descending* priority. Lower-priority zones are evicted
/// first when the budget is tight (spec 05). The order of the variants encodes
/// the priority: earlier = more important = evicted later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Zone {
    /// Role, current step, tool schemas. Fixed and minimal. Sacred.
    System,
    /// The user's original request, verbatim. Sacred — prevents goal drift.
    TaskAnchor,
    /// The FULL current contents of the file(s) being edited (the focus files). Sacred: on an
    /// edit task this is the single most essential context — if it's clipped the model edits a
    /// truncated view, can't anchor `old_str`, and thrashes (observed live: the 30B looped
    /// read→edit→write_file on a 790-line terrain.rs whose focus pin kept getting evicted).
    FocusFile,
    /// The most recent tool result the model must react to. Sacred.
    RecentObservation,
    /// Retrieved snippets relevant to the current step. Budgeted, evictable.
    Retrieved,
    /// Compacted summary of older turns. Lowest priority, evicted first.
    HistorySummary,
}

impl Zone {
    /// Sacred zones are never evicted to make room (spec 05 — "what stays sacred").
    pub fn is_sacred(self) -> bool {
        matches!(
            self,
            Zone::System | Zone::TaskAnchor | Zone::FocusFile | Zone::RecentObservation
        )
    }
}

/// One piece of content tagged with its zone and the chat role it becomes.
#[derive(Debug, Clone)]
pub struct Segment {
    pub zone: Zone,
    pub role: Role,
    pub text: String,
}

/// Which chat role a segment is rendered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Segment {
    pub fn system(zone: Zone, text: impl Into<String>) -> Self {
        Self {
            zone,
            role: Role::System,
            text: text.into(),
        }
    }
    pub fn user(zone: Zone, text: impl Into<String>) -> Self {
        Self {
            zone,
            role: Role::User,
            text: text.into(),
        }
    }
    pub fn assistant(zone: Zone, text: impl Into<String>) -> Self {
        Self {
            zone,
            role: Role::Assistant,
            text: text.into(),
        }
    }

    fn to_message(&self) -> Message {
        match self.role {
            Role::System => Message::system(self.text.clone()),
            Role::User => Message::user(self.text.clone()),
            Role::Assistant => Message::assistant(self.text.clone()),
        }
    }
}

/// The result of assembling a prompt under budget.
#[derive(Debug, Clone)]
pub struct BuiltContext {
    /// The messages to send, in stable prompt order.
    pub messages: Vec<Message>,
    /// Total tokens the assembled prompt is estimated/counted to use.
    pub tokens_used: usize,
    /// The hard budget it was fit into.
    pub budget: usize,
    /// Zones dropped entirely under budget pressure (for logging/inspection).
    pub dropped: Vec<Zone>,
}

/// Truncate `text` to roughly `target_tokens`, keeping its HEAD and TAIL with a marked cut in
/// the middle (a source file's imports/types sit up top; the region being edited is often near
/// the end where new code lands). Line-granular so anchors stay copyable. If `text` already
/// fits, it's returned unchanged.
fn truncate_middle(text: &str, target_tokens: usize, counter: &TokenCounter) -> String {
    if counter.count(text) <= target_tokens || target_tokens == 0 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let marker = "\n… (file truncated in the middle to fit the context window) …\n";
    if lines.len() < 8 {
        // Too few lines to middle-truncate by line — fall back to a char-level head+tail cut so a
        // large single-block observation still shrinks (else the prompt can't be made to fit).
        let chars: Vec<char> = text.chars().collect();
        // Budget the MARKER first, then MEASURE -- do not estimate.
        //
        // Two bugs lived here. The keep was `target * 4` chars while
        // `estimate_tokens` charges 3.5, so every shrink landed ABOVE the target;
        // and the marker's own cost was never counted, which dominates at a small
        // target. Either way the result exceeded the target, the caller's shrink
        // loop saw `after >= before`, broke out to avoid spinning, and the prompt
        // went to the server over budget -- 30,668 tokens against a 12,288 budget,
        // rejected with HTTP 400 as "exceeds the available context size".
        //
        // Any chars-per-token constant is approximate, and one that lands high does
        // not merely waste room: it stops the caller converging. So halve the kept
        // content until the measured candidate actually fits.
        let marker_cost = counter.count(marker);
        let content_tokens = target_tokens.saturating_sub(marker_cost);
        let mut keep = (content_tokens.saturating_mul(3)).min(chars.len());
        if keep + marker.len() >= chars.len() {
            return text.to_string();
        }
        loop {
            let half = keep / 2;
            let head: String = chars[..half].iter().collect();
            let tail: String = chars[chars.len() - (keep - half)..].iter().collect();
            let candidate = format!("{head}{marker}{tail}");
            // `keep == 0` is the floor: the marker alone. That may still exceed a
            // target smaller than the marker itself, which is unachievable by
            // construction -- but it is strictly smaller than the input, so the
            // caller's loop still makes progress.
            if keep == 0 || counter.count(&candidate) <= target_tokens {
                return candidate;
            }
            keep /= 2;
        }
    }
    // Grow head and tail line counts until we'd exceed the budget, then step back one.
    let mut head = 0usize;
    let mut tail = 0usize;
    loop {
        // Alternate growing head and tail for a balanced keep.
        let grow_head = head <= tail;
        let (nh, nt) = if grow_head {
            (head + 1, tail)
        } else {
            (head, tail + 1)
        };
        if nh + nt >= lines.len() {
            break;
        }
        let candidate = format!(
            "{}{marker}{}",
            lines[..nh].join("\n"),
            lines[lines.len() - nt..].join("\n")
        );
        if counter.count(&candidate) > target_tokens {
            break;
        }
        head = nh;
        tail = nt;
    }
    if head == 0 && tail == 0 {
        // Even the marker + a line each is over budget — keep just the head that fits.
        return lines[..lines.len().min(1)].join("\n");
    }
    format!(
        "{}{marker}{}",
        lines[..head].join("\n"),
        lines[lines.len() - tail..].join("\n")
    )
}

/// Compute the hard prompt budget from the backend's advertised window.
///
/// We budget against an **effective** fraction of the nominal window (small
/// models degrade well before the advertised max, spec 05) and subtract a reserve
/// for the model's reply.
pub fn prompt_budget(
    max_context_tokens: usize,
    effective_fraction: f64,
    response_reserve: usize,
) -> usize {
    let effective = (max_context_tokens as f64 * effective_fraction) as usize;
    // The reserve may never take more than HALF the usable window.
    //
    // A reserve sized for a 32k model is catastrophic on an 8k one: 8192 * 0.75 is
    // 6144, so a 6144-token reserve leaves a budget of ZERO -- and a zero budget does
    // not fail loudly, it stops constraining anything, and the prompt grows until the
    // server rejects it. That bug was fixed once by detecting the backend's real
    // window, and then reintroduced the moment the reserve default was raised to suit
    // the model actually in use.
    //
    // Capping here rather than at each call site means no caller can recreate it: the
    // reserve is a REQUEST, the window is the constraint, and the constraint wins. A
    // model whose replies genuinely need more than half its window needs a bigger
    // window, not a bigger reserve.
    let capped_reserve = response_reserve.min(effective / 2);
    effective.saturating_sub(capped_reserve)
}

/// Per-message chat-template cost, re-exported for the cost function above.
fn sc_context_message_overhead() -> usize {
    crate::tokens::MESSAGE_OVERHEAD_TOKENS
}

/// Assembles segments into a budgeted prompt (spec 05).
pub struct ContextBuilder<'a> {
    counter: &'a TokenCounter<'a>,
    budget: usize,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(counter: &'a TokenCounter<'a>, budget: usize) -> Self {
        Self { counter, budget }
    }

    /// Fit `segments` into the budget, evicting whole non-sacred segments from the
    /// lowest priority up until it fits. Sacred segments are always kept (if the
    /// sacred set alone exceeds budget, they're kept anyway — truncation of their
    /// *contents* is the truncation layer's job, not eviction's).
    ///
    /// Prompt order is stable and zone-sorted (System → TaskAnchor → … → History)
    /// regardless of eviction, so the model always sees a consistent layout.
    pub fn build(&self, mut segments: Vec<Segment>) -> BuiltContext {
        // Stable order for the final prompt: by zone priority, preserving input
        // order within a zone.
        segments.sort_by_key(|s| s.zone);

        // Cost a segment as the REQUEST will carry it: its text plus the chat
        // template markup wrapped around every message. Omitting the wrapper is
        // how a prompt the harness measured at 26,516 tokens reached the server as
        // 34,237 and was rejected -- the budget was being enforced against a
        // number that did not exist.
        let cost = |s: &Segment| self.counter.count(&s.text) + sc_context_message_overhead();
        let total: usize = segments.iter().map(cost).sum();

        let mut dropped = Vec::new();
        if total > self.budget {
            // Evict lowest-priority (highest Zone value) non-sacred segments first.
            // Walk zones from least to most important.
            let mut running = total;
            // Candidate indices sorted by *descending* priority (evict-first order).
            let mut evict_order: Vec<usize> = (0..segments.len()).collect();
            evict_order.sort_by(|&i, &j| segments[j].zone.cmp(&segments[i].zone));

            let mut evicted = vec![false; segments.len()];
            for idx in evict_order {
                if running <= self.budget {
                    break;
                }
                if segments[idx].zone.is_sacred() {
                    continue;
                }
                running -= cost(&segments[idx]);
                evicted[idx] = true;
                if !dropped.contains(&segments[idx].zone) {
                    dropped.push(segments[idx].zone);
                }
            }
            // Keep only non-evicted, preserving order.
            let mut kept = Vec::new();
            for (i, seg) in segments.into_iter().enumerate() {
                if !evicted[i] {
                    kept.push(seg);
                }
            }
            segments = kept;
        }

        // Last resort: even after evicting every non-sacred zone, the sacred content can exceed
        // the window — a large FocusFile (the file being edited) plus a full recent window. A
        // sacred zone is never dropped, but an over-budget one must be TRUNCATED, or the request
        // overflows the model's context and the backend rejects it outright (observed live:
        // 34445 tokens vs a 24576 window → HTTP 400). Shrink the biggest FocusFile segment to
        // fit, keeping its head and tail (imports/types up top, the code being edited often near
        // where the model is working) with a marked cut.
        let mut running: usize = segments.iter().map(cost).sum();
        while running > self.budget {
            // Shrink the LARGEST truncatable sacred segment. Prefer the focus file / observations
            // (bulky, safe to middle-clip); the System and TaskAnchor zones are the true minimum
            // and only clipped if nothing else is left. A single large read observation or a huge
            // grounded task instruction can each blow the window, so we can't restrict this to the
            // FocusFile zone alone (observed live: multiple full-file reads in the recent window →
            // 35k tokens vs a 24576 window → HTTP 400).
            let truncatable = |z: Zone| matches!(z, Zone::FocusFile | Zone::RecentObservation);
            let pick = segments
                .iter()
                .enumerate()
                .filter(|(_, s)| truncatable(s.zone) && cost(s) > 32)
                .max_by_key(|(_, s)| cost(s))
                .or_else(|| {
                    // Last resort: the biggest sacred segment of any zone (System/TaskAnchor too).
                    segments
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.zone.is_sacred() && cost(s) > 32)
                        .max_by_key(|(_, s)| cost(s))
                })
                .map(|(i, _)| i);
            let Some(idx) = pick else {
                // Nothing left big enough to shrink. "Send it and let the backend
                // cope" was the old behaviour here, and the backend does NOT cope:
                // it returns HTTP 400 and the whole turn is lost, which the runner
                // reports as a solver error and reads as the model's fault.
                //
                // Reaching here means the sacred content alone exceeds the budget
                // even after every truncatable zone was clipped -- so the budget is
                // too small for the task, not the prompt too large. There is nothing
                // more this function can drop; the caller is told by `tokens_used`
                // exceeding `budget` in the returned context, and the agent loop
                // raises a harness fault on it.
                break;
            };
            let over = running - self.budget;
            let before = cost(&segments[idx]);
            // Shrink toward what this segment must give up, but NEVER ask for zero.
            //
            // `before - over` goes negative -- and saturates to 0 -- whenever the
            // overage exceeds this one segment, which is the normal case when the
            // prompt is over budget by more than its largest piece. `truncate_middle`
            // treats a target of 0 as "no limit given" and returns the text
            // unchanged, so the loop saw `after >= before`, gave up, and sent the
            // prompt whole: measured 33,080 tokens against a 12,288 budget, rejected
            // by the server as over-context.
            //
            // Halving instead means each pass makes real progress and the loop
            // converges over several segments rather than demanding one segment
            // absorb the entire overage.
            let target = before.saturating_sub(over + 64).max(before / 2).max(1);
            segments[idx].text = truncate_middle(&segments[idx].text, target, self.counter);
            let after = cost(&segments[idx]);
            if after >= before {
                break; // couldn't shrink further — avoid an infinite loop
            }
            running = running - before + after;
        }

        let tokens_used = segments.iter().map(cost).sum();
        let messages = segments.iter().map(Segment::to_message).collect();
        BuiltContext {
            messages,
            tokens_used,
            budget: self.budget,
            dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter() -> TokenCounter<'static> {
        TokenCounter::estimator()
    }

    #[test]
    fn prompt_budget_applies_fraction_and_reserve() {
        // 8192 * 0.75 = 6144, minus 1024 reserve = 5120.
        assert_eq!(prompt_budget(8192, 0.75, 1024), 5120);
    }

    /// **A reserve can never consume the window.**
    ///
    /// It used to clamp to 0, and a zero budget does not fail loudly -- it stops
    /// constraining anything, nothing is evicted, and the prompt grows until the
    /// server rejects it. That is not a theoretical case: a reserve sized for a 32k
    /// model (6144) applied to an 8k one gives 8192 * 0.75 - 6144 = exactly 0.
    #[test]
    fn the_reserve_can_never_take_the_whole_window() {
        // The 32k-sized reserve against a small window: half the usable space is
        // kept for the prompt rather than none of it.
        assert_eq!(prompt_budget(8192, 0.75, 6144), 3072);
        // An absurd reserve is capped, not honoured.
        assert_eq!(prompt_budget(1000, 0.5, 9999), 250);
        // Every window leaves a usable prompt budget.
        for window in [4096usize, 8192, 16384, 32768, 128_000] {
            let b = prompt_budget(window, 0.75, 6144);
            assert!(b > 0, "window {window} produced a zero budget");
        }
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let c = counter();
        let b = ContextBuilder::new(&c, 10_000);
        let built = b.build(vec![
            Segment::system(Zone::System, "you are an agent"),
            Segment::user(Zone::TaskAnchor, "fix the bug"),
            Segment::user(Zone::Retrieved, "fn foo() {}"),
        ]);
        assert_eq!(built.messages.len(), 3);
        assert!(built.dropped.is_empty());
        assert!(built.tokens_used <= built.budget);
    }

    #[test]
    fn evicts_lowest_priority_first_under_pressure() {
        let c = counter();
        // Budget only big enough for the sacred zones + maybe a bit. Each message
        // also carries the chat template's per-message markup, which the builder
        // charges because the SERVER does -- so the budget must cover it too, or
        // this asks the impossible rather than testing eviction order.
        let per_msg = crate::tokens::MESSAGE_OVERHEAD_TOKENS;
        let sacred_cost = crate::tokens::estimate_tokens("you are an agent")
            + crate::tokens::estimate_tokens("fix the bug")
            + crate::tokens::estimate_tokens("last tool output")
            + 3 * per_msg;
        let b = ContextBuilder::new(&c, sacred_cost + 2);

        let big_history = "summary ".repeat(50);
        let big_retrieved = "code ".repeat(50);
        let built = b.build(vec![
            Segment::system(Zone::System, "you are an agent"),
            Segment::user(Zone::TaskAnchor, "fix the bug"),
            Segment::user(Zone::RecentObservation, "last tool output"),
            Segment::user(Zone::Retrieved, big_retrieved),
            Segment::user(Zone::HistorySummary, big_history),
        ]);

        // History (lowest) goes first; if still over, Retrieved too. Sacred kept.
        assert!(built.dropped.contains(&Zone::HistorySummary));
        assert!(built.tokens_used <= built.budget);
        // The three sacred zones survived (content preserved verbatim).
        let kept: Vec<&str> = built.messages.iter().map(|m| m.content.as_str()).collect();
        assert!(kept.contains(&"you are an agent"));
        assert!(kept.contains(&"fix the bug"));
        assert!(kept.contains(&"last tool output"));
    }

    #[test]
    fn focus_file_is_sacred_and_survives_eviction() {
        // The edit-task fix: the file being edited (Zone::FocusFile) must never be evicted, even
        // when Retrieved/History are dropped for budget. Otherwise the model edits a truncated
        // view and can't anchor old_str.
        let c = counter();
        let sacred = crate::tokens::estimate_tokens("sys")
            + crate::tokens::estimate_tokens("task")
            + crate::tokens::estimate_tokens("obs")
            + crate::tokens::estimate_tokens(&"the whole file ".repeat(30));
        let b = ContextBuilder::new(&c, sacred + 1);
        let built = b.build(vec![
            Segment::system(Zone::System, "sys"),
            Segment::user(Zone::TaskAnchor, "task"),
            Segment::user(Zone::FocusFile, "the whole file ".repeat(30)),
            Segment::user(Zone::RecentObservation, "obs"),
            Segment::user(Zone::Retrieved, "evictable retrieved ".repeat(30)),
            Segment::user(Zone::HistorySummary, "evictable history ".repeat(30)),
        ]);
        assert!(Zone::FocusFile.is_sacred());
        let kept: String = built.messages.iter().map(|m| m.content.clone()).collect();
        assert!(kept.contains("the whole file"), "focus file survived");
        assert!(
            built.dropped.contains(&Zone::Retrieved)
                || built.dropped.contains(&Zone::HistorySummary)
        );
    }

    #[test]
    fn never_evicts_sacred_even_if_over_budget() {
        let c = counter();
        // Budget of 1 token: impossible, but sacred zones must still be present.
        let b = ContextBuilder::new(&c, 1);
        let built = b.build(vec![
            Segment::system(Zone::System, "system"),
            Segment::user(Zone::TaskAnchor, "task"),
            Segment::user(Zone::RecentObservation, "obs"),
            Segment::user(Zone::HistorySummary, "history that should go"),
        ]);
        // History dropped; the three sacred remain even though we're over budget.
        assert_eq!(built.messages.len(), 3);
        assert!(built.dropped.contains(&Zone::HistorySummary));
    }

    #[test]
    fn a_huge_focus_file_is_truncated_to_fit_rather_than_overflowing() {
        // The live crash: a large (sacred) FocusFile pushed the prompt past the window and the
        // backend rejected the whole request (HTTP 400). Now the focus file is truncated to fit,
        // keeping head + tail, so the request always fits — sacred means "kept, possibly clipped,"
        // never "overflow the window."
        let c = counter();
        let budget = 200;
        let b = ContextBuilder::new(&c, budget);
        let huge: String = (0..500)
            .map(|i| format!("line number {i} of the focus file\n"))
            .collect();
        let built = b.build(vec![
            Segment::system(Zone::System, "sys"),
            Segment::user(Zone::TaskAnchor, "task"),
            Segment::user(Zone::FocusFile, huge),
            Segment::user(Zone::RecentObservation, "obs"),
        ]);
        assert!(
            built.tokens_used <= budget,
            "prompt fits the window: {} <= {budget}",
            built.tokens_used
        );
        // All four zones are still present (nothing dropped), the focus file just got clipped.
        assert_eq!(built.messages.len(), 4);
        let focus = &built.messages[2].content;
        assert!(
            focus.contains("truncated in the middle"),
            "focus file clipped: {focus}"
        );
        // Head and tail are both preserved.
        assert!(focus.contains("line number 0 "), "head kept");
        assert!(focus.contains("line number 499"), "tail kept");
    }

    #[test]
    fn a_huge_recent_observation_is_also_truncated_to_fit() {
        // The 2nd overflow bug: several full-file reads pile into the sacred recent window and
        // exceed the window even after the focus file is clipped. Any large sacred segment
        // (not just FocusFile) must shrink so the prompt fits.
        let c = counter();
        let budget = 200;
        let b = ContextBuilder::new(&c, budget);
        let huge_read: String = (0..500).map(|i| format!("read line {i}\n")).collect();
        let built = b.build(vec![
            Segment::system(Zone::System, "sys"),
            Segment::user(Zone::TaskAnchor, "task"),
            Segment::user(Zone::RecentObservation, huge_read),
        ]);
        assert!(
            built.tokens_used <= budget,
            "fits: {} <= {budget}",
            built.tokens_used
        );
        assert_eq!(built.messages.len(), 3, "nothing dropped, just clipped");
        assert!(
            built.messages[2].content.contains("truncated"),
            "recent obs clipped"
        );
    }

    #[test]
    fn output_is_ordered_by_zone_priority() {
        let c = counter();
        let b = ContextBuilder::new(&c, 10_000);
        // Supplied out of order; output must be System, TaskAnchor, Retrieved.
        let built = b.build(vec![
            Segment::user(Zone::Retrieved, "retrieved"),
            Segment::system(Zone::System, "system"),
            Segment::user(Zone::TaskAnchor, "task"),
        ]);
        assert_eq!(built.messages[0].content, "system");
        assert_eq!(built.messages[1].content, "task");
        assert_eq!(built.messages[2].content, "retrieved");
    }

    /// **The builder must fit the budget, even when everything is sacred.**
    ///
    /// A single huge unbreakable observation -- one long line, no newlines to cut
    /// on -- takes the char-level fallback in `truncate_middle`. That fallback used
    /// to land above its target, so the shrink loop gave up and the prompt was sent
    /// over budget: 30,668 tokens against 12,288, rejected with HTTP 400.
    #[test]
    fn a_huge_single_line_sacred_segment_is_still_made_to_fit() {
        let c = counter();
        let b = ContextBuilder::new(&c, 200);

        // One line, far over budget, in a sacred zone: nothing may be evicted and
        // there are no line breaks to truncate on.
        let built = b.build(vec![
            Segment::system(Zone::System, "sys"),
            Segment::user(Zone::TaskAnchor, "task"),
            Segment::user(Zone::RecentObservation, "x".repeat(20_000)),
        ]);

        assert!(
            built.tokens_used <= built.budget,
            "assembled {} tokens against a {}-token budget",
            built.tokens_used,
            built.budget
        );
    }

    /// The same for a multi-line file, which takes the line-based path.
    #[test]
    fn a_huge_focus_file_is_still_made_to_fit() {
        let c = counter();
        let b = ContextBuilder::new(&c, 300);

        let file: String = (0..2_000)
            .map(|i| format!("let value_{i} = compute_something({i});\n"))
            .collect();
        let built = b.build(vec![
            Segment::system(Zone::System, "sys"),
            Segment::user(Zone::TaskAnchor, "task"),
            Segment::user(Zone::FocusFile, file),
        ]);

        assert!(
            built.tokens_used <= built.budget,
            "assembled {} tokens against a {}-token budget",
            built.tokens_used,
            built.budget
        );
    }

    /// `truncate_middle` must land at or below its target whenever that is
    /// achievable -- i.e. once the target can pay for the marker.
    ///
    /// Landing above is what made the shrink loop give up (`after >= before`), so
    /// this is not a rounding nicety: it is the difference between the prompt
    /// converging and being sent over budget.
    #[test]
    fn truncate_middle_does_not_overshoot_an_achievable_target() {
        let c = counter();
        let marker_cost =
            c.count("\n… (file truncated in the middle to fit the context window) …\n");

        // No newlines: forces the char-level fallback, which is where the bug was.
        let blob = "abcdefghij".repeat(3_000);
        for target in [50usize, 200, 1000] {
            assert!(
                target > marker_cost,
                "premise: the target can pay for the marker"
            );
            let got = truncate_middle(&blob, target, &c);
            assert!(
                c.count(&got) <= target,
                "target {target}, got {} tokens",
                c.count(&got)
            );
        }

        // Below the marker's own cost the best it can do is the marker -- but it
        // must still SHRINK, or the caller's loop stalls.
        let tiny = truncate_middle(&blob, 5, &c);
        assert!(
            c.count(&tiny) < c.count(&blob),
            "must always make progress, even on an impossible target"
        );
    }

    /// **The assembled prompt must fit, no matter how the overage is distributed.**
    ///
    /// This is the shape that broke live: a big focus file plus MANY recent
    /// observations, so the total is over budget by far more than any single
    /// segment. The shrink loop computed `target = before - over`, which saturates
    /// to 0 whenever the overage exceeds that segment -- and `truncate_middle`
    /// treats 0 as "no limit", returning the text unchanged. The loop then saw
    /// `after >= before`, gave up, and sent the prompt whole: 33,080 tokens against
    /// a 12,288 budget, rejected by the server as over-context and reported as the
    /// model's failure.
    #[test]
    fn a_prompt_over_budget_by_more_than_its_largest_segment_still_fits() {
        let c = counter();
        let b = ContextBuilder::new(&c, 12_288);
        let mut segs = vec![
            Segment::system(Zone::System, "sys ".repeat(500)),
            Segment::user(Zone::TaskAnchor, "task ".repeat(2000)),
            Segment::user(
                Zone::FocusFile,
                "line of code
"
                .repeat(3000),
            ),
        ];
        for _ in 0..20 {
            segs.push(Segment::user(Zone::RecentObservation, "obs ".repeat(800)));
        }
        let built = b.build(segs);
        eprintln!(
            "BUILT tokens_used={} budget={}",
            built.tokens_used, built.budget
        );
        assert!(
            built.tokens_used <= built.budget,
            "{} > {}",
            built.tokens_used,
            built.budget
        );
    }
}
