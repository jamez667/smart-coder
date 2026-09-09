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
use sc_model::{Message, ToolCallRecord};

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
    /// Append the assistant action + its observation as a new turn, with the action
    /// stored as PLAIN TEXT. The `ParseRepair` / `Grammar` path: the model genuinely
    /// produced text, so it replays as text, byte-identically to how it always has.
    ///
    /// The loop always calls [`Self::push_turn_with_calls`] (it has the reply's calls,
    /// which are empty on those paths); this is the same thing spelled for tests.
    #[cfg(test)]
    pub(super) fn push_turn(&mut self, action: &str, observation: &str) {
        self.push_turn_with_calls(action, &[], observation);
    }

    /// The same, but carrying the NATIVE tool calls the backend returned for this turn.
    ///
    /// This is the round trip that keeps a tool-tagged chat template honest. With calls
    /// present the turn is stored as an assistant message that still carries them, and
    /// the observation is stored as a `tool` RESULT paired to the first call's id — the
    /// shape the OpenAI wire format and every ChatML-family template expect. The server's
    /// template then re-emits the model's own `<tool_call>…</tool_call><|im_end|>`
    /// markup, so on turn N the model reads a history in the format it actually writes.
    ///
    /// With `calls` empty this is exactly [`Self::push_turn`]: nothing about the
    /// text path changes.
    pub(super) fn push_turn_with_calls(
        &mut self,
        action: &str,
        calls: &[ToolCallRecord],
        observation: &str,
    ) {
        // Build the action FIRST and pair the observation to what it actually kept.
        // `assistant_with_calls` drops the whole turn's calls when any one of them has
        // arguments that are not valid JSON (a reply truncated at the token cap, which the
        // harness still salvages usable work from). When that happens the turn must revert
        // ENTIRELY to the pre-fix shape: a `tool` result whose `tool_call_id` names a call
        // no longer in the history is malformed in its own right, and servers reject it.
        // So the pairing reads `has_tool_calls()`, never the `calls` argument.
        let action_msg = if calls.is_empty() {
            Message::assistant(action.to_string())
        } else {
            Message::assistant_with_calls(action.to_string(), calls.to_vec())
        };
        let observation = if action_msg.has_tool_calls() {
            // The result pairs to the FIRST call: the harness executes exactly one tool
            // per turn, so a reply carrying several is the model over-answering and only
            // the first is run. Pairing the observation to a call that never ran would
            // be a worse lie than not pairing it at all.
            Message::tool(action_msg.tool_calls[0].wire_id(), observation.to_string())
        } else {
            Message::user(observation.to_string())
        };
        let action = action_msg;
        self.turns.push(Turn {
            action,
            notes: vec![observation],
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
        sc_model::Role::Tool => "tool",
    }
}

pub(super) fn seg_from_message(zone: Zone, m: &Message) -> Segment {
    match m.role {
        sc_model::Role::System => Segment::system(zone, m.content.clone()),
        sc_model::Role::User => Segment::user(zone, m.content.clone()),
        sc_model::Role::Assistant if m.tool_calls.is_empty() => {
            Segment::assistant(zone, m.content.clone())
        }
        sc_model::Role::Assistant => {
            Segment::assistant_with_calls(zone, m.content.clone(), m.tool_calls.clone())
        }
        sc_model::Role::Tool => Segment::tool(
            zone,
            m.tool_call_id.clone().unwrap_or_default(),
            m.content.clone(),
        ),
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

    /// **A native call is stored as a call, and its observation as a paired result.**
    ///
    /// This is the harness half of the Mellum2 fix. The window used to store the
    /// backend's flattened `{"tool":...}` string as plain assistant content, so the next
    /// request replayed it as prose -- a format the model's own chat template never
    /// emits, and one with no stop token in it. Stored as a call, the server's template
    /// re-emits the model's own `<tool_call>...</tool_call><|im_end|>` markup instead.
    #[test]
    fn a_native_call_is_stored_as_a_call_with_its_result_paired_to_it() {
        let mut w = RecentWindow::default();
        let call = ToolCallRecord::new("call_1", "read_file", r#"{"path":"lib.rs"}"#);
        w.push_turn_with_calls(
            r#"{"tool":"read_file","path":"lib.rs"}"#,
            std::slice::from_ref(&call),
            "read_file lib.rs:\nfn main() {}",
        );

        let msgs: Vec<&Message> = w.messages().collect();
        assert_eq!(msgs.len(), 2);

        // The action still carries the normalised text every extractor reads...
        assert_eq!(msgs[0].role, sc_model::Role::Assistant);
        assert_eq!(msgs[0].content, r#"{"tool":"read_file","path":"lib.rs"}"#);
        // ...AND the structured call the next request replays.
        assert_eq!(msgs[0].tool_calls, vec![call.clone()]);

        // The observation is a tool RESULT naming the call it answers.
        assert_eq!(msgs[1].role, sc_model::Role::Tool);
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("call_1"));
        assert!(msgs[1].content.contains("fn main"));

        // And the segments the prompt builder gets carry both through.
        let segs: Vec<sc_context::Segment> = w
            .messages()
            .map(|m| seg_from_message(Zone::RecentObservation, m))
            .collect();
        assert_eq!(segs[0].tool_calls, vec![call]);
        assert_eq!(segs[1].role, sc_context::Role::Tool);
        assert_eq!(segs[1].tool_call_id.as_deref(), Some("call_1"));
    }

    /// **The text path is byte-identical to before.** `ParseRepair` and `Grammar`
    /// produce no native call, so their turns must store exactly as they always have --
    /// a plain assistant message and a plain user observation. Anything else would
    /// change Tiel's prompt and invalidate every recorded run.
    #[test]
    fn a_text_only_turn_stores_exactly_as_it_always_has() {
        let mut w = RecentWindow::default();
        w.push_turn_with_calls(r#"{"tool":"finish"}"#, &[], "done");

        let msgs: Vec<&Message> = w.messages().collect();
        assert_eq!(msgs[0].role, sc_model::Role::Assistant);
        assert!(msgs[0].tool_calls.is_empty());
        assert!(msgs[0].tool_call_id.is_none());
        assert_eq!(msgs[1].role, sc_model::Role::User, "NOT a tool result");
        assert!(msgs[1].tool_call_id.is_none());

        // Identical to what the old two-argument form produced.
        let mut old = RecentWindow::default();
        old.push_turn(r#"{"tool":"finish"}"#, "done");
        let a: Vec<(sc_model::Role, &str)> =
            w.messages().map(|m| (m.role, m.content.as_str())).collect();
        let b: Vec<(sc_model::Role, &str)> = old
            .messages()
            .map(|m| (m.role, m.content.as_str()))
            .collect();
        assert_eq!(a, b);
    }

    /// A call the server gave no id for still pairs: both halves derive the same
    /// stand-in from the call itself, via the one `wire_id` in `sc_model`.
    #[test]
    fn a_call_with_no_server_id_still_pairs_to_its_result() {
        let mut w = RecentWindow::default();
        let call = ToolCallRecord::new("", "finish", r#"{"summary":"done"}"#);
        w.push_turn_with_calls(r#"{"tool":"finish"}"#, std::slice::from_ref(&call), "ok");
        let msgs: Vec<&Message> = w.messages().collect();
        assert_eq!(
            msgs[1].tool_call_id.as_deref(),
            Some(call.wire_id().as_str())
        );
        assert!(!call.wire_id().is_empty());
    }
}
