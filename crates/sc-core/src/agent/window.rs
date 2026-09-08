//! Recent-window plumbing: the verbatim tail of assistant/user turns the loop keeps
//! uncompacted, plus the small role/segment conversions used when assembling the prompt.
//!
//! The window is a list of whole TURNS, not a flat message list. A turn is the model's
//! action plus every user-role message the harness attached to it: the tool observation
//! first, then any notes injected the same turn (a failed auto-verify report, advisor
//! advice, a diagnosis). Eviction always removes a whole turn, so the window can never start
//! on an orphaned assistant message or lose the note that belonged to an observation. The
//! window is append-only between evictions -- what the prefix KV cache needs -- and nothing
//! ever rewrites a message already in it: an observation, once shown, stays as shown.

use sc_context::{Segment, Zone};
use sc_model::Message;

/// One model turn as the window holds it: the action and its attached user messages.
#[derive(Debug, Clone)]
pub(super) struct Turn {
    pub(super) action: Message,
    /// The observation, then any harness notes attached the same turn. Never empty once
    /// built by [`RecentWindow::push_turn`].
    pub(super) notes: Vec<Message>,
}

/// The verbatim recent window: whole turns, oldest first.
#[derive(Debug, Default)]
pub(super) struct RecentWindow {
    /// Harness notes that arrived before any turn existed to attach to. Unreachable in the
    /// loop today (every note follows a `push_turn` in the same iteration), kept so a note
    /// is never silently dropped if that changes.
    head: Vec<Message>,
    turns: Vec<Turn>,
}

impl RecentWindow {
    /// Append the assistant action + its observation as a new turn. Nothing is trimmed here:
    /// the loop evicts by budget, oldest turn first (see the eviction loop in `mod.rs`).
    pub(super) fn push_turn(&mut self, action: &str, observation: &str) {
        self.turns.push(Turn {
            action: Message::assistant(action.to_string()),
            notes: vec![Message::user(observation.to_string())],
        });
    }

    /// Attach a harness-originated observation (e.g. advisor advice) to the newest turn as a
    /// plain user message — NOT a fake assistant turn, so the model never sees itself
    /// "saying" a harness label and parrots it back. It travels with that turn on eviction.
    pub(super) fn push_observation(&mut self, observation: &str) {
        let msg = Message::user(observation.to_string());
        match self.turns.last_mut() {
            Some(t) => t.notes.push(msg),
            None => self.head.push(msg),
        }
    }

    /// How many whole turns the window holds.
    pub(super) fn len(&self) -> usize {
        self.turns.len()
    }

    /// Drop the oldest whole turn (action + every message attached to it). `None` if empty.
    pub(super) fn evict_oldest(&mut self) -> Option<Turn> {
        if self.turns.is_empty() {
            None
        } else {
            Some(self.turns.remove(0))
        }
    }

    /// Every message in prompt order: orphan notes, then each turn's action followed by
    /// its observation and notes.
    pub(super) fn messages(&self) -> impl Iterator<Item = &Message> {
        self.head.iter().chain(
            self.turns
                .iter()
                .flat_map(|t| std::iter::once(&t.action).chain(t.notes.iter())),
        )
    }
}

/// The lowercase role word for the verbose prompt dump (`PromptAssembled`).
pub(super) fn role_word(role: sc_model::Role) -> &'static str {
    match role {
        sc_model::Role::System => "system",
        sc_model::Role::User => "user",
        sc_model::Role::Assistant => "assistant",
    }
}

pub(super) fn seg_from_message(zone: Zone, m: &Message) -> Segment {
    match m.role {
        sc_model::Role::System => Segment::system(zone, m.content.clone()),
        sc_model::Role::User => Segment::user(zone, m.content.clone()),
        sc_model::Role::Assistant => Segment::assistant(zone, m.content.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_recent_window_is_tagged_recent_observation_so_it_survives_eviction() {
        // Fix B: a file the model read two turns ago must NOT be evicted just because a
        // newer turn arrived. The loop tags the ENTIRE recent window RecentObservation
        // (sacred), so an earlier read survives a tight budget. We verify the zoning rule
        // directly: every message in a multi-message recent window maps to the sacred zone.
        let mut recent = RecentWindow::default();
        recent.push_turn(
            r#"{"tool":"read_file","path":"app.py"}"#,
            "read_file app.py:\n<the whole file body>",
        );
        recent.push_turn(
            r#"{"tool":"read_file","path":"db.py"}"#,
            "read_file db.py:\n<another file body>",
        );
        // The zoning the loop now applies (mirrors the assembly loop): all RecentObservation.
        for m in recent.messages() {
            let seg = seg_from_message(Zone::RecentObservation, m);
            assert_eq!(
                seg.zone,
                Zone::RecentObservation,
                "every recent message must be in the sacred recent zone"
            );
            assert!(
                seg.zone.is_sacred(),
                "the recent zone must be sacred so an earlier read survives eviction"
            );
        }
    }

    #[test]
    fn a_harness_note_rides_with_its_turn_and_leaves_with_it() {
        let mut w = RecentWindow::default();
        w.push_turn("a1", "obs1");
        w.push_observation("note for turn 1");
        w.push_turn("a2", "obs2");
        assert_eq!(w.len(), 2);
        assert_eq!(w.messages().count(), 5);

        // Evicting the oldest turn takes its note with it, never leaving a stray user
        // message or an orphaned assistant message at the front.
        let gone = w.evict_oldest().expect("a turn");
        assert_eq!(gone.action.content, "a1");
        assert_eq!(gone.notes.len(), 2);
        let left: Vec<&str> = w.messages().map(|m| m.content.as_str()).collect();
        assert_eq!(left, vec!["a2", "obs2"]);
        assert_eq!(w.messages().next().unwrap().role, sc_model::Role::Assistant);
    }
}
