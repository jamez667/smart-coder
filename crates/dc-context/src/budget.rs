//! The zoned, hard-budgeted prompt assembler (spec 05 — the budget).
//!
//! Every prompt is built to fit a hard token budget = an *effective* fraction of
//! the backend's advertised window, minus a reserve for the model's reply. The
//! budget is split into priority-ordered zones; under pressure the manager evicts
//! from the lowest priority up and **never** drops the sacred zones (task anchor,
//! current step, most recent observation).

use dc_model::Message;

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
            Zone::System | Zone::TaskAnchor | Zone::RecentObservation
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
    effective.saturating_sub(response_reserve)
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

        let cost = |s: &Segment| self.counter.count(&s.text);
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
        // Reserve larger than effective clamps to 0, never underflows.
        assert_eq!(prompt_budget(1000, 0.5, 9999), 0);
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
        // Budget only big enough for the sacred zones + maybe a bit.
        let sacred_cost = crate::tokens::estimate_tokens("you are an agent")
            + crate::tokens::estimate_tokens("fix the bug")
            + crate::tokens::estimate_tokens("last tool output");
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
}
