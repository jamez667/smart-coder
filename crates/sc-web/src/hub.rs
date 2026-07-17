//! The event hub: fan a single agent run's [`AgentEvent`] stream out to every
//! connected browser, and the Server-Sent Events (SSE) wire format.
//!
//! The agent runs on one thread and pushes events here; each connected SSE client
//! has a queue the hub appends to. A late-joining client gets the full backlog so
//! it can rebuild the current view, then live events thereafter — so refreshing
//! the page mid-run Just Works.

use std::sync::{Arc, Mutex};

use sc_core::{AgentEvent, EventSink};

/// One SSE-framed message: `data: <json>\n\n` (the SSE protocol).
pub fn sse_frame(event: &AgentEvent) -> String {
    let json = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    // SSE data may not contain bare newlines; our JSON is single-line, but guard
    // anyway by prefixing each line with `data: `.
    let mut out = String::new();
    for line in json.lines() {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Shared, thread-safe broadcast buffer. Every event is appended; clients read by
/// index so each sees the whole ordered stream exactly once.
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
}

#[derive(Default)]
struct HubInner {
    events: Vec<AgentEvent>,
    /// Set once the run ends, so a client knows to stop polling.
    done: bool,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an event to the broadcast buffer.
    pub fn push(&self, event: AgentEvent) {
        let mut g = self.inner.lock().unwrap();
        if matches!(event, AgentEvent::Stopped { .. }) {
            g.done = true;
        }
        g.events.push(event);
    }

    /// Events at or after `from`, plus the next read index and whether the run is
    /// done. A client calls this in a loop, advancing `from`.
    pub fn since(&self, from: usize) -> (Vec<AgentEvent>, usize, bool) {
        let g = self.inner.lock().unwrap();
        let start = from.min(g.events.len());
        let slice = g.events[start..].to_vec();
        let next = g.events.len();
        (slice, next, g.done)
    }

    /// Whether the run has ended.
    pub fn is_done(&self) -> bool {
        self.inner.lock().unwrap().done
    }

    /// Total events seen so far.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An [`EventSink`] that feeds a [`Hub`] — handed to the agent loop.
pub struct HubSink {
    hub: Hub,
}

impl HubSink {
    pub fn new(hub: Hub) -> Self {
        Self { hub }
    }
}

impl EventSink for HubSink {
    fn record(&self, event: &AgentEvent) {
        self.hub.push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_core::StopReason;

    #[test]
    fn sse_frame_is_well_formed() {
        let f = sse_frame(&AgentEvent::ToolCall {
            tool: "read_file".into(),
            arg: "a.rs".into(),
        });
        assert!(f.starts_with("data: "), "{f}");
        assert!(f.ends_with("\n\n"), "{f:?}");
        assert!(f.contains("\"type\":\"ToolCall\""), "{f}");
        assert!(f.contains("\"tool\":\"read_file\""), "{f}");
    }

    #[test]
    fn hub_buffers_and_replays_in_order() {
        let hub = Hub::new();
        hub.push(AgentEvent::RunStarted {
            task: "t".into(),
            prompt_budget: 100,
        });
        hub.push(AgentEvent::ToolCall {
            tool: "x".into(),
            arg: "y".into(),
        });

        // A fresh client reads from 0 — gets the whole backlog.
        let (batch, next, done) = hub.since(0);
        assert_eq!(batch.len(), 2);
        assert_eq!(next, 2);
        assert!(!done);

        // Reading again from `next` yields nothing new.
        let (batch2, next2, _) = hub.since(next);
        assert!(batch2.is_empty());
        assert_eq!(next2, 2);
    }

    #[test]
    fn stopped_event_marks_done() {
        let hub = Hub::new();
        assert!(!hub.is_done());
        hub.push(AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        assert!(hub.is_done());
        let (_, _, done) = hub.since(0);
        assert!(done);
    }

    #[test]
    fn hub_sink_forwards_into_the_hub() {
        let hub = Hub::new();
        let sink = HubSink::new(hub.clone());
        sink.record(&AgentEvent::ModelTurn {
            step: 1,
            prompt_tokens: 50,
            raw: String::new(),
        });
        assert_eq!(hub.len(), 1);
    }

    #[test]
    fn since_past_the_end_is_safe() {
        let hub = Hub::new();
        hub.push(AgentEvent::ModelTurn {
            step: 1,
            prompt_tokens: 1,
            raw: String::new(),
        });
        // Asking from beyond the end returns empty, not a panic.
        let (batch, next, _) = hub.since(999);
        assert!(batch.is_empty());
        assert_eq!(next, 1);
    }
}
