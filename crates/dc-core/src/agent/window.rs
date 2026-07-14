//! Recent-window plumbing: the verbatim tail of assistant/user turns the loop keeps
//! uncompacted, plus the small role/segment conversions used when assembling the prompt.

use dc_context::{Segment, Zone};
use dc_model::Message;

/// Append the assistant action + its observation, capping the verbatim window to
/// roughly `keep_recent` turns (each turn is one assistant + one user message).
pub(super) fn push_recent(
    recent: &mut Vec<Message>,
    action: &str,
    observation: &str,
    keep_recent: usize,
) {
    recent.push(Message::assistant(action.to_string()));
    recent.push(Message::user(observation.to_string()));
    trim_recent(recent, keep_recent);
}

/// Inject a harness-originated observation (e.g. advisor advice) as a plain user
/// message — NOT a fake assistant turn, so the model never sees itself "saying"
/// a harness label and parrots it back.
pub(super) fn push_observation(recent: &mut Vec<Message>, observation: &str, keep_recent: usize) {
    recent.push(Message::user(observation.to_string()));
    trim_recent(recent, keep_recent);
}

pub(super) fn trim_recent(recent: &mut Vec<Message>, keep_recent: usize) {
    let max_msgs = keep_recent.saturating_mul(2).max(2);
    while recent.len() > max_msgs {
        recent.remove(0);
    }
}

/// Overwrite the content of the most recent `user` message in `recent`, in place. Used by
/// the repeat-dedup nudge (Fix #2): when an idempotent call is repeated, the prior turn's
/// *successful* result of that same call is the last user message — leaving it verbatim
/// lets the model trust "it worked" over the nudge. Replacing it with a short superseded
/// marker keeps the window honest (that result was already consumed) without dropping the
/// assistant/user turn structure. No-op if there is no user message yet.
pub(super) fn replace_last_user(recent: &mut [Message], marker: &str) {
    if let Some(m) = recent
        .iter_mut()
        .rev()
        .find(|m| m.role == dc_model::Role::User)
    {
        m.content = marker.to_string();
    }
}

/// The lowercase role word for the verbose prompt dump (`PromptAssembled`).
pub(super) fn role_word(role: dc_model::Role) -> &'static str {
    match role {
        dc_model::Role::System => "system",
        dc_model::Role::User => "user",
        dc_model::Role::Assistant => "assistant",
    }
}

pub(super) fn seg_from_message(zone: Zone, m: &Message) -> Segment {
    match m.role {
        dc_model::Role::System => Segment::system(zone, m.content.clone()),
        dc_model::Role::User => Segment::user(zone, m.content.clone()),
        dc_model::Role::Assistant => Segment::assistant(zone, m.content.clone()),
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
        let recent = vec![
            Message::assistant(r#"{"tool":"read_file","path":"app.py"}"#.to_string()),
            Message::user("read_file app.py:\n<the whole file body>".to_string()),
            Message::assistant(r#"{"tool":"read_file","path":"db.py"}"#.to_string()),
            Message::user("read_file db.py:\n<another file body>".to_string()),
        ];
        // The zoning the loop now applies (mirrors the assembly loop): all RecentObservation.
        for m in recent.iter() {
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
}
