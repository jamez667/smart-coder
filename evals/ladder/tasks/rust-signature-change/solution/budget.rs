//! The retry-budget calculation. Shared by every caller that plans retries.

/// How many attempts a job gets, and how long to wait between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub attempts: u32,
    pub backoff_ms: u64,
}

/// Plan the retry budget for a job.
///
/// `weight` is the job's cost class (1 = cheap, higher = more expensive). A more
/// expensive job gets fewer attempts and waits longer between them.
///
/// `deadline_ms` is the wall-clock window the whole retry schedule must fit
/// inside: attempts are cut until `attempts * backoff_ms <= deadline_ms`, with a
/// floor of one attempt (a job always gets at least one try).
pub fn plan(weight: u32, deadline_ms: u64) -> Budget {
    let mut attempts = if weight >= 4 { 2 } else { 5 };
    let backoff_ms = 100 * weight as u64;
    while attempts > 1 && attempts as u64 * backoff_ms > deadline_ms {
        attempts -= 1;
    }
    Budget { attempts, backoff_ms }
}
