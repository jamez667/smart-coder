//! `sc-daemon` — file a request from anywhere, come back to a drafted spec
//! (specs 18/19/20, first pass).
//!
//! **Autonomy is in the scheduling, never in the approval.** The runner drafts a
//! spec and stops. It never passes a gate.
//!
//! This first pass is deliberately narrower than spec 19 describes, and the
//! narrowing is what makes it safe to expose:
//!
//! * **Specs only.** The daemon drafts Phase 1 and nothing else. It is *unable* to
//!   reach architecture, layout, stage breakdown, decomposition or the build path
//!   — not by policy but by construction, because the only [`WorkflowMode`] it
//!   ever builds stops after the specs phase. Spec 19 insists the "no writing
//!   code" anti-goal be structural rather than a policy line; this is what that
//!   looks like.
//! * **Approving starts nothing.** It writes the spec into the repository and
//!   marks the task `Ready`. The developer builds it in their IDE when they
//!   choose.
//! * **Any repository.** The daemon serves whatever the developer configures in
//!   `~/.smart-coder/daemon.json`. Nothing here assumes a particular workspace.
//!
//! [`WorkflowMode`]: sc_workflow::WorkflowMode

pub mod atomic;
pub mod config;
pub mod feedback;
pub mod intake;
pub mod park;
pub mod preflight;
pub mod queue;
pub mod runner;
pub mod task;

#[cfg(test)]
mod test_support;

pub use config::{DaemonConfig, Repo};
pub use feedback::{Feedback, FeedbackStore};
pub use intake::IntakeKind;
pub use park::ParkingGate;
pub use preflight::NotReady;
pub use queue::Queue;
pub use runner::{approve, discard, draft, draft_next, send_back, Drafted};
pub use task::{Task, TaskState};
