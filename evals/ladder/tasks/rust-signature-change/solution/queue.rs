//! The in-process job queue. Two entry points, two kinds of job.

#[path = "budget.rs"]
pub mod budget;

use budget::{plan, Budget};

/// A health probe must answer inside the liveness window or the supervisor
/// restarts us, so a probe's whole retry schedule has to fit in one second.
pub const PROBE_DEADLINE_MS: u64 = 1_000;

/// A queued unit of work.
#[derive(Debug, Clone)]
pub struct Job {
    pub name: String,
    /// Cost class: 1 = cheap, higher = more expensive.
    pub weight: u32,
    pub budget: Budget,
}

/// Enqueue an ordinary job at its own cost class.
///
/// An ordinary job carries a deadline chosen by whoever submitted it.
pub fn enqueue(name: &str, weight: u32, deadline_ms: u64) -> Job {
    Job { name: name.to_string(), weight, budget: plan(weight, deadline_ms) }
}

/// Enqueue a probe: a tiny health check, always the cheapest class, and bound by
/// the liveness window rather than by any caller.
pub fn enqueue_probe(name: &str) -> Job {
    Job { name: name.to_string(), weight: 1, budget: plan(1, PROBE_DEADLINE_MS) }
}
