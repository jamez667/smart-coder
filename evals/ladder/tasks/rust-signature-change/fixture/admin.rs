//! Operator-triggered work. Whatever an operator asks for runs at the heaviest
//! cost class, because it is always a one-off and never a hot path.

#[path = "queue.rs"]
pub mod queue;

use queue::budget::{plan, Budget};

/// The cost class every operator-triggered job runs at.
pub const ADMIN_WEIGHT: u32 = 8;

/// An operator sits and watches a rebuild, so it may take as long as five
/// minutes before we give up on it.
pub const ADMIN_DEADLINE_MS: u64 = 300_000;

/// The budget for an operator-triggered rebuild.
pub fn rebuild_budget() -> Budget {
    plan(ADMIN_WEIGHT)
}
