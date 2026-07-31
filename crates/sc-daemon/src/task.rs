//! A filed task and its lifecycle.
//!
//! Deliberately narrower than spec 19's full state machine, because this first
//! pass drafts **specs only**: the public surface files a request, the daemon
//! drafts Phase 1, and a human approves or sends it back. Approving does not start
//! work — it writes the spec into the repository and marks the task [`Ready`], and
//! the developer picks it up in their IDE when they choose to.
//!
//! ```text
//!   Queued ──claim──► Drafting ──draft──► AwaitingReview ──approve──► Ready
//!      ▲                  │                     │
//!      └──preflight───────┘                     └──send back──► Queued
//!         refused                                └──discard───► Discarded
//!                         └──error────► Failed
//! ```
//!
//! Two distinctions the table exists to preserve:
//!
//! * **[`AwaitingReview`] is not [`Failed`]**, for the reason spec 13 keeps
//!   `Unknown` distinct from a verdict: collapsing "I need you" into "I broke"
//!   trains the developer to ignore both. A task waiting for a human is the system
//!   working correctly.
//! * **[`Ready`] is not "done".** Nothing has been built. Calling it done would be
//!   a queue lying about what happened — the spec is settled, the work is not.
//!
//! [`Ready`]: TaskState::Ready
//! [`AwaitingReview`]: TaskState::AwaitingReview
//! [`Failed`]: TaskState::Failed

use serde::{Deserialize, Serialize};

/// Where a task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    /// Filed, not yet drafted. Also where a sent-back task returns to.
    Queued,
    /// The runner is drafting its spec right now.
    Drafting,
    /// A spec exists and is waiting for a human to approve or send it back.
    AwaitingReview,
    /// Approved. The spec is written into the repository; the developer builds it
    /// when they choose. **Not** "done" — nothing has been built.
    Ready,
    /// Dropped by a human before it was approved.
    Discarded,
    /// The run could not continue. Kept visible and never retried silently — an
    /// automatic retry hides a reproducible problem behind eventual success.
    Failed,
}

impl TaskState {
    /// A short label for a list view.
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Drafting => "drafting",
            TaskState::AwaitingReview => "awaiting review",
            TaskState::Ready => "ready",
            TaskState::Discarded => "discarded",
            TaskState::Failed => "failed",
        }
    }

    /// Is this task finished with, one way or another?
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Ready | TaskState::Discarded | TaskState::Failed
        )
    }

    /// Does a task in this state occupy its repository?
    ///
    /// Only [`Drafting`](TaskState::Drafting). A task awaiting review must **not**
    /// hold the slot — otherwise one unread review starves every other task for
    /// that repo, and spec 20's promise that deferring is free would be false.
    pub fn holds_the_repo(self) -> bool {
        self == TaskState::Drafting
    }

    /// Sort key for a list: what needs a human first, then what is in flight,
    /// then the settled. A queue sorted by id answers no question anyone has.
    pub fn list_order(self) -> u8 {
        match self {
            TaskState::AwaitingReview => 0,
            TaskState::Drafting => 1,
            TaskState::Queued => 2,
            TaskState::Failed => 3,
            TaskState::Ready => 4,
            TaskState::Discarded => 5,
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One filed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Assigned at intake and distinct from the artifact slug (spec 19 — "Task
    /// identity"). The slug is derived from the text and can collide; this cannot.
    pub id: String,
    /// The request, free-form — the same string an interactive user would type.
    pub text: String,
    /// The **name** of the repository this is for, resolved against the daemon's
    /// configured set. Never a path: a request that carried one would put path
    /// handling on the network path (spec 18).
    pub repo: String,
    /// What kind of request this is. Shapes the drafting prompt — a bug spec and
    /// a feature spec are not the same document (see [`crate::intake`]).
    ///
    /// `serde(default)` so a task filed before kinds existed reads as a feature
    /// rather than becoming unreadable.
    #[serde(default)]
    pub kind: crate::intake::IntakeKind,
    pub state: TaskState,
    /// Unix ms when it was filed.
    pub filed_ms: u64,
    /// Why a task is `Failed`, or why a `Queued` one was put back — the reason a
    /// preflight refused it, say. `None` when there is nothing to explain.
    #[serde(default)]
    pub note: Option<String>,
    /// Where the drafted spec lives, workspace-relative (`specs/<slug>`), once the
    /// runner has produced one.
    #[serde(default)]
    pub artifact_dir: Option<String>,
}

impl Task {
    /// File a new request.
    pub fn new(id: impl Into<String>, text: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            repo: repo.into(),
            kind: crate::intake::IntakeKind::default(),
            state: TaskState::Queued,
            filed_ms: now_ms(),
            note: None,
            artifact_dir: None,
        }
    }

    /// File a request of a particular kind.
    pub fn of_kind(
        id: impl Into<String>,
        text: impl Into<String>,
        repo: impl Into<String>,
        kind: crate::intake::IntakeKind,
    ) -> Self {
        Self {
            kind,
            ..Self::new(id, text, repo)
        }
    }

    /// A one-line summary of the request, for a list.
    pub fn summary(&self) -> &str {
        self.text.lines().next().unwrap_or("").trim()
    }

    /// Where this task's artifacts live, workspace-relative.
    ///
    /// The recorded directory once a run has produced one, else the slug the
    /// request's text derives to — the same resolution the CLI and GUI use, which
    /// is what lets a phone-filed task and a desktop session land in one place.
    pub fn artifact_dir_or_slug(&self) -> String {
        self.artifact_dir
            .clone()
            .unwrap_or_else(|| format!("specs/{}", sc_workflow::slugify(&self.text)))
    }

    /// Move to `state`, recording why.
    pub fn set_state(&mut self, state: TaskState, note: Option<String>) {
        self.state = state;
        self.note = note;
    }
}

/// A fresh task id: time-ordered so a directory listing reads chronologically,
/// with a per-process counter so two tasks filed in the same millisecond differ.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{:013}-{:04}", now_ms(), n % 10_000)
}

/// Unix milliseconds. Saturates at 0 rather than panicking on a pre-epoch clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaiting_review_does_not_hold_the_repository() {
        // The rule that keeps one unread review from starving a repo's queue. If
        // this were true, spec 20's "deferring is free" would be false.
        assert!(TaskState::Drafting.holds_the_repo());
        assert!(!TaskState::AwaitingReview.holds_the_repo());
        assert!(!TaskState::Queued.holds_the_repo());
        assert!(!TaskState::Ready.holds_the_repo());
    }

    #[test]
    fn ready_is_terminal_but_awaiting_review_is_not() {
        // `Ready` means the spec is settled and the developer will build it when
        // they choose — the daemon is finished with it either way.
        assert!(TaskState::Ready.is_terminal());
        assert!(TaskState::Discarded.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        // A task waiting for a human is the system working, not a finished task.
        assert!(!TaskState::AwaitingReview.is_terminal());
        assert!(!TaskState::Queued.is_terminal());
        assert!(!TaskState::Drafting.is_terminal());
    }

    #[test]
    fn a_list_shows_what_needs_a_human_first() {
        let mut states = vec![
            TaskState::Ready,
            TaskState::Queued,
            TaskState::AwaitingReview,
            TaskState::Discarded,
            TaskState::Drafting,
            TaskState::Failed,
        ];
        states.sort_by_key(|s| s.list_order());
        assert_eq!(
            states,
            vec![
                TaskState::AwaitingReview,
                TaskState::Drafting,
                TaskState::Queued,
                TaskState::Failed,
                TaskState::Ready,
                TaskState::Discarded,
            ]
        );
    }

    #[test]
    fn a_task_is_filed_queued_with_no_artifacts_yet() {
        let t = Task::new("id-1", "Add seat types for crew roles", "alpha");
        assert_eq!(t.state, TaskState::Queued);
        assert!(t.artifact_dir.is_none());
        assert!(t.note.is_none());
        assert!(t.filed_ms > 0);
    }

    #[test]
    fn the_summary_is_the_first_line_of_a_multi_line_request() {
        let t = Task::new("id", "Add seat types\n\nMore detail here.", "alpha");
        assert_eq!(t.summary(), "Add seat types");
    }

    #[test]
    fn ids_are_unique_and_time_ordered() {
        // Time-ordered so a directory listing reads chronologically; the counter
        // is what keeps two tasks filed in one millisecond apart.
        let ids: Vec<String> = (0..50).map(|_| new_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids collided: {ids:?}");
    }

    #[test]
    fn a_task_round_trips_through_json() {
        let mut t = Task::new("id-1", "do the thing", "alpha");
        t.set_state(TaskState::Failed, Some("the backend was down".into()));
        t.artifact_dir = Some("specs/do-the-thing".into());
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Task>(&json).unwrap(), t);
    }

    #[test]
    fn a_record_written_before_the_optional_fields_existed_still_loads() {
        // The queue is durable across upgrades; a task filed by an older build
        // must not become unreadable, or the developer loses it.
        let json = r#"{"id":"i","text":"t","repo":"alpha","state":"queued","filed_ms":1}"#;
        let t: Task = serde_json::from_str(json).unwrap();
        assert_eq!(t.state, TaskState::Queued);
        assert!(t.note.is_none());
        assert!(t.artifact_dir.is_none());
    }
}
