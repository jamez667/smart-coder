//! `sc-review` — post-integration review (spec 16).
//!
//! A second gate over the **integrated diff**, after verification and before a run
//! is reported done. Tests answer *does it work?* and answer it well; this asks
//! *should this code stay?* — did the worker duplicate a helper it couldn't find,
//! swallow an error to make an assertion pass, or make tangential changes nobody
//! asked for. None of those are visible to a test suite: the duplicate passes its
//! own tests, a swallowed error *is* a passing test, and tangential correct code
//! is still green.
//!
//! Two rules constrain everything here, and neither may be traded away for
//! convenience:
//!
//! 1. **Review never rewrites code.** A finding is evidence handed to a decision,
//!    never an edit. There is no seam in this crate through which a reviewer
//!    could modify the workspace, and there must never be one.
//! 2. **Only a *corroborated* finding may block or feed a retry.** An
//!    uncorroborated model opinion is reported and ranked and can never stop a
//!    run. Reviewer agreement ranks a finding; it never promotes an opinion to a
//!    fact, because correlated models can be confidently wrong together. That
//!    rule lives in exactly one place — [`Finding::may_act`].
//!
//! The shape:
//!
//! ```text
//! integrated diff ──┬─► ground (repo map + pre-retrieved symbols) ──┐
//!                   │                                              ▼
//!                   │                        lenses × reviewers (parallel)
//!                   │                                              │
//!                   └─────────────► corroborate ◄──────────────────┘
//!                                        │
//!                                   merge votes → rank
//! ```
//!
//! Engine only — no CLI and no UI, mirroring `sc-verify` and `sc-comply`. The
//! swarm wires it into `integrate_with_retry`; the CLI renders its events.

pub mod corroborate;
pub mod diff;
pub mod engine;
pub mod finding;
pub mod ground;
pub mod lens;
pub mod rank;

#[cfg(test)]
mod test_support;

pub use corroborate::Corroboration;
pub use diff::{FileDiff, Hunk, HunkId, IntegratedDiff};
pub use engine::{review, Action, ReviewConfig, ReviewOutcome, Reviewer};
pub use finding::{Anchor, Finding, Lens, ModelId, Severity};
pub use ground::{ground, Grounding, SimilarSymbol};
pub use rank::{blocking, merge_votes, rank, retry_feedback};
