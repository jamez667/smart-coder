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
pub fn plan(weight: u32) -> Budget {
    let attempts = if weight >= 4 { 2 } else { 5 };
    Budget { attempts, backoff_ms: 100 * weight as u64 }
}
