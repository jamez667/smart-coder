//! The task board (spec 08 — blackboard): the orchestrator-owned set of
//! subtasks, their status, and the dependency DAG between them.
//!
//! This is the durable, inspectable state of a swarm run — not held in any
//! model's head. It's pure data: the scheduler asks it "what's ready to run?",
//! marks subtasks claimed/done/failed, and it answers whether the whole task is
//! complete. Independent subtasks (no unmet deps) are returned together so the
//! orchestrator can run them concurrently.

use serde::{Deserialize, Serialize};

/// A subtask's lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Not yet started (may be waiting on dependencies).
    Pending,
    /// Handed to a worker, in progress.
    Claimed,
    /// Completed and its proposed change accepted into the workspace.
    Done,
    /// The worker failed or its change was rejected at integration.
    Failed,
}

/// One unit of work for a single worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtask {
    /// Stable id (used for deps + reporting).
    pub id: String,
    /// The scoped goal handed to the worker (spec 08 — tight, single-purpose).
    pub goal: String,
    /// Files the worker is expected to touch (hint for scoping; not enforced).
    pub files: Vec<String>,
    /// Ids of subtasks that must be `Done` before this one can run.
    pub deps: Vec<String>,
    pub status: Status,
}

impl Subtask {
    pub fn new(id: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            goal: goal.into(),
            files: Vec::new(),
            deps: Vec::new(),
            status: Status::Pending,
        }
    }

    pub fn with_files(mut self, files: Vec<String>) -> Self {
        self.files = files;
        self
    }

    pub fn with_deps(mut self, deps: Vec<String>) -> Self {
        self.deps = deps;
        self
    }
}

/// The orchestrator's task board.
#[derive(Debug, Clone, Default)]
pub struct TaskBoard {
    subtasks: Vec<Subtask>,
}

impl TaskBoard {
    pub fn new(subtasks: Vec<Subtask>) -> Self {
        Self { subtasks }
    }

    pub fn is_empty(&self) -> bool {
        self.subtasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.subtasks.len()
    }

    pub fn subtasks(&self) -> &[Subtask] {
        &self.subtasks
    }

    fn get(&self, id: &str) -> Option<&Subtask> {
        self.subtasks.iter().find(|s| s.id == id)
    }

    fn status_of(&self, id: &str) -> Option<Status> {
        self.get(id).map(|s| s.status)
    }

    /// Ids of subtasks ready to run *now*: pending, with every dependency `Done`.
    /// These have no inter-dependencies among themselves, so the orchestrator may
    /// run them all concurrently (spec 08 — parallel independent subtasks).
    pub fn ready(&self) -> Vec<String> {
        self.subtasks
            .iter()
            .filter(|s| s.status == Status::Pending)
            .filter(|s| {
                s.deps
                    .iter()
                    .all(|d| self.status_of(d) == Some(Status::Done))
            })
            .map(|s| s.id.clone())
            .collect()
    }

    /// Mark a subtask claimed (handed to a worker).
    pub fn claim(&mut self, id: &str) {
        self.set(id, Status::Claimed);
    }

    /// Mark a subtask done (its change was accepted).
    pub fn complete(&mut self, id: &str) {
        self.set(id, Status::Done);
    }

    /// Mark a subtask failed (worker failed or change rejected).
    pub fn fail(&mut self, id: &str) {
        self.set(id, Status::Failed);
    }

    fn set(&mut self, id: &str, status: Status) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| s.id == id) {
            s.status = status;
        }
    }

    /// True when no subtask can make further progress: nothing pending is runnable
    /// (everything is done/failed, or remaining pending tasks are permanently
    /// blocked by failed deps). The scheduler stops when this holds.
    pub fn is_quiescent(&self) -> bool {
        let any_active = self.subtasks.iter().any(|s| s.status == Status::Claimed);
        !any_active && self.ready().is_empty()
    }

    /// Every subtask reached `Done`.
    pub fn all_done(&self) -> bool {
        !self.subtasks.is_empty() && self.subtasks.iter().all(|s| s.status == Status::Done)
    }

    /// Counts by status, for reporting.
    pub fn tally(&self) -> (usize, usize, usize) {
        let done = self.count(Status::Done);
        let failed = self.count(Status::Failed);
        let pending = self.subtasks.len() - done - failed;
        (done, failed, pending)
    }

    fn count(&self, status: Status) -> usize {
        self.subtasks.iter().filter(|s| s.status == status).count()
    }

    /// A compact rendering for logs/inspection.
    pub fn render(&self) -> String {
        let mut s = String::from("task board:");
        for t in &self.subtasks {
            let glyph = match t.status {
                Status::Pending => "[ ]",
                Status::Claimed => "[~]",
                Status::Done => "[x]",
                Status::Failed => "[!]",
            };
            s.push_str(&format!("\n  {glyph} {}: {}", t.id, t.goal));
            if !t.deps.is_empty() {
                s.push_str(&format!("  (after {})", t.deps.join(", ")));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> TaskBoard {
        // a, b independent; c depends on both.
        TaskBoard::new(vec![
            Subtask::new("a", "do a"),
            Subtask::new("b", "do b"),
            Subtask::new("c", "do c").with_deps(vec!["a".into(), "b".into()]),
        ])
    }

    #[test]
    fn ready_returns_independent_subtasks() {
        let b = board();
        let mut ready = b.ready();
        ready.sort();
        assert_eq!(ready, vec!["a", "b"]); // c is blocked
    }

    #[test]
    fn dependent_becomes_ready_only_after_deps_done() {
        let mut b = board();
        b.complete("a");
        assert_eq!(b.ready(), vec!["b"]); // c still needs b
        b.complete("b");
        assert_eq!(b.ready(), vec!["c"]); // now unblocked
    }

    #[test]
    fn claim_removes_from_ready() {
        let mut b = board();
        b.claim("a");
        assert_eq!(b.ready(), vec!["b"]);
        assert!(!b.is_quiescent()); // a is claimed/active
    }

    #[test]
    fn all_done_and_tally() {
        let mut b = board();
        b.complete("a");
        b.complete("b");
        b.complete("c");
        assert!(b.all_done());
        assert_eq!(b.tally(), (3, 0, 0));
    }

    #[test]
    fn quiescent_when_a_dep_failed_blocks_the_rest() {
        let mut b = board();
        b.fail("a"); // c can never run now
        b.complete("b");
        // c is pending but its dep `a` failed -> never ready -> quiescent.
        assert!(b.ready().is_empty());
        assert!(b.is_quiescent());
        assert!(!b.all_done());
        assert_eq!(b.tally(), (1, 1, 1)); // b done, a failed, c pending
    }

    #[test]
    fn render_shows_status_and_deps() {
        let r = board().render();
        assert!(r.contains("[ ] a: do a"), "{r}");
        assert!(r.contains("(after a, b)"), "{r}");
    }
}
