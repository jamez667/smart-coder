//! The run log — a centralized, in-process record of everything that happened in a run,
//! with deterministic (code, not LLM) search/trace (spec 01 — the event stream is the
//! durable state; this retains and queries it).
//!
//! Every meaningful step already emits a structured, serializable [`AgentEvent`] through an
//! [`EventSink`] — but the events were emitted and DROPPED. So a consumer that needed an
//! earlier result (e.g. the diagnostic sub-agent wanting the last test output) had to RE-RUN
//! to recover it. The run log keeps the stream and answers precise questions about it with
//! code, so the harness can hand a model only the relevant slice instead of re-running or
//! dumping a 5000-line haystack. This is the "Loki, built in" pattern: because the store IS
//! the NDJSON-serializable `AgentEvent` stream, an external Loki/Promtail is just another sink
//! over the same data — no coupling here.

use std::sync::{Mutex, MutexGuard};

use crate::event::{AgentEvent, EventSink};

/// An ordered, queryable record of a run's events.
#[derive(Debug, Default, Clone)]
pub struct RunLog {
    events: Vec<AgentEvent>,
}

impl RunLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// All events, in the order they happened.
    pub fn events(&self) -> &[AgentEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn push(&mut self, e: AgentEvent) {
        self.events.push(e);
    }

    // ---- deterministic query/trace (the in-process LogQL) ----

    /// The full output of the most recent verification, if any. Removes the need to re-run
    /// the suite to recover output the harness already saw.
    pub fn last_verification(&self) -> Option<&str> {
        self.events.iter().rev().find_map(|e| match e {
            AgentEvent::Verification { full, .. } => Some(full.as_str()),
            _ => None,
        })
    }

    /// The full output of the most recent RED (failing) verification — the precise slice the
    /// diagnostic sub-agent reasons over. `None` if no failing verification was recorded.
    pub fn slice_for_diagnosis(&self) -> Option<&str> {
        self.events.iter().rev().find_map(|e| match e {
            AgentEvent::Verification { green: false, full, .. } => Some(full.as_str()),
            _ => None,
        })
    }

    /// Every verification event, in order — for tracing how the suite evolved over the run.
    pub fn verifications(&self) -> Vec<&AgentEvent> {
        self.events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Verification { .. }))
            .collect()
    }

    /// Every tool call whose key argument mentions `path` — trace "what happened to file X".
    pub fn tool_calls_for_path(&self, path: &str) -> Vec<&AgentEvent> {
        self.events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { arg, .. } if arg.contains(path)))
            .collect()
    }
}

/// An [`EventSink`] that appends every event into a [`RunLog`]. Compose it via
/// [`crate::event::TeeSink`] alongside the real sink to capture the whole stream with no
/// per-emit change, and query it MID-RUN (the diagnostic does). The `Mutex` keeps it `Sync`
/// like the other sinks (the loop may run on a worker thread sharing `&dyn EventSink`); a
/// poisoned lock is recovered rather than panicking — observation must never break a run.
pub struct RunLogSink {
    inner: Mutex<RunLog>,
}

impl RunLogSink {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RunLog::new()),
        }
    }

    /// Lock the log to query it (recovers a poisoned lock).
    pub fn lock(&self) -> MutexGuard<'_, RunLog> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the accumulated log (e.g. after the run, to persist or inspect).
    pub fn into_inner(self) -> RunLog {
        self.inner.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for RunLogSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for RunLogSink {
    fn record(&self, event: &AgentEvent) {
        if let Ok(mut g) = self.inner.lock() {
            g.push(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::StopReason;

    fn verif(green: bool, full: &str) -> AgentEvent {
        AgentEvent::Verification {
            green,
            summary: full.lines().next().unwrap_or("").to_string(),
            full: full.to_string(),
        }
    }

    #[test]
    fn records_every_event_in_order() {
        let sink = RunLogSink::new();
        sink.record(&AgentEvent::RunStarted {
            task: "t".into(),
            prompt_budget: 100,
        });
        sink.record(&verif(false, "1 failed"));
        sink.record(&AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        let log = sink.lock();
        assert_eq!(log.len(), 3);
        assert!(matches!(log.events()[0], AgentEvent::RunStarted { .. }));
        assert!(matches!(log.events()[2], AgentEvent::Stopped { .. }));
    }

    #[test]
    fn last_verification_returns_the_latest_full() {
        let sink = RunLogSink::new();
        sink.record(&verif(false, "OLD output"));
        sink.record(&verif(true, "NEW output"));
        // A later non-verification event must not change the answer.
        sink.record(&AgentEvent::ToolCall {
            tool: "read_file".into(),
            arg: "a.py".into(),
        });
        assert_eq!(sink.lock().last_verification(), Some("NEW output"));
    }

    #[test]
    fn slice_for_diagnosis_returns_the_last_red_only() {
        let sink = RunLogSink::new();
        sink.record(&verif(false, "RED one"));
        sink.record(&verif(false, "RED two"));
        sink.record(&verif(true, "green now"));
        // The latest RED, not the latest verification (which is green).
        assert_eq!(sink.lock().slice_for_diagnosis(), Some("RED two"));

        // Only-green ⇒ no slice.
        let only_green = RunLogSink::new();
        only_green.record(&verif(true, "all good"));
        assert_eq!(only_green.lock().slice_for_diagnosis(), None);
    }

    #[test]
    fn tool_calls_for_path_traces_a_file() {
        let sink = RunLogSink::new();
        for arg in ["a.py", "b.py", "a.py"] {
            sink.record(&AgentEvent::ToolCall {
                tool: "edit_file".into(),
                arg: arg.into(),
            });
        }
        assert_eq!(sink.lock().tool_calls_for_path("a.py").len(), 2);
        assert_eq!(sink.lock().tool_calls_for_path("c.py").len(), 0);
    }
}
