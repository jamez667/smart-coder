//! Renders a one-line budget summary for the status page.

#[path = "admin.rs"]
pub mod admin;

use admin::queue::budget::plan;

/// The status page recomputes every 30s, so nothing it summarises may plan a
/// schedule longer than that.
pub const STATUS_PAGE_DEADLINE_MS: u64 = 30_000;

/// Summarise the budget a job of `weight` would get, as `"<n>x/<ms>ms"`.
pub fn summarise(weight: u32) -> String {
    let b = plan(weight);
    format!("{}x/{}ms", b.attempts, b.backoff_ms)
}
