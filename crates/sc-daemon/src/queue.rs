//! The durable queue.
//!
//! **On disk, not in memory.** A daemon that loses its queue to a power cut is one
//! nobody trusts with a task filed from a train (spec 19).
//!
//! One JSON file per task under `~/.smart-coder/queue/`, each written atomically.
//! A file per task rather than one queue file is what makes concurrent readers
//! safe without a lock: the CLI listing tasks while the runner claims one touches
//! different files, and a torn write can at worst lose the *one* task being
//! written rather than the whole queue.
//!
//! Ordering is by task id, which is time-prefixed — so `pop the oldest` is a sort,
//! not a scheduler. Spec 19 is explicit that this is a queue and not a CI system:
//! no priorities, no fairness, no preemption.

use std::path::{Path, PathBuf};

use sc_proto::{DcError, Result};

use crate::task::{Task, TaskState};

/// A queue rooted at a directory.
#[derive(Debug, Clone)]
pub struct Queue {
    dir: PathBuf,
}

impl Queue {
    /// Open (or create) the queue at `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Queue> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Queue { dir })
    }

    /// The daemon's default queue, `~/.smart-coder/queue/`.
    pub fn default_queue() -> Result<Queue> {
        Queue::open(crate::config::home().join("queue"))
    }

    /// Where the queue lives.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// File a task.
    pub fn put(&self, task: &Task) -> Result<()> {
        let json = serde_json::to_string_pretty(task).map_err(|e| DcError::Eval(e.to_string()))?;
        crate::atomic::write(&self.path_for(&task.id), json.as_bytes())
    }

    /// Read one task.
    pub fn get(&self, id: &str) -> Result<Option<Task>> {
        match std::fs::read_to_string(self.path_for(id)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| DcError::Eval(format!("task {id} is unreadable: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Read one task, or say it is not there.
    pub fn require(&self, id: &str) -> Result<Task> {
        self.get(id)?
            .ok_or_else(|| DcError::Eval(format!("no task {id:?} in the queue")))
    }

    /// Every task, oldest first.
    ///
    /// A record that no longer parses is **skipped rather than fatal**: one
    /// corrupt file must not hide every other task from the developer. It is
    /// reported through [`unreadable`](Queue::unreadable) instead, so the
    /// corruption is visible without being blocking.
    pub fn all(&self) -> Result<Vec<Task>> {
        let mut out: Vec<Task> = self
            .ids()?
            .into_iter()
            .filter_map(|id| self.get(&id).ok().flatten())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Task ids whose records could not be parsed — surfaced so a corrupt queue
    /// entry is *visible* rather than silently absent.
    pub fn unreadable(&self) -> Result<Vec<String>> {
        Ok(self
            .ids()?
            .into_iter()
            .filter(|id| self.get(id).is_err())
            .collect())
    }

    fn ids(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = std::fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(str::to_string)
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Tasks in a given state, oldest first.
    pub fn in_state(&self, state: TaskState) -> Result<Vec<Task>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|t| t.state == state)
            .collect())
    }

    /// The next task to draft: the oldest `Queued` one whose repository is free.
    ///
    /// **Serialised per repository, not per runner** (spec 19). A repo is busy
    /// only while something is `Drafting` — a task awaiting review does *not* hold
    /// it, or one unread review would starve every other task for that repo and
    /// spec 20's promise that deferring is free would be untrue.
    ///
    /// Skipping a busy repo rather than stopping is deliberate: work for a free
    /// repo should not wait behind work for a busy one.
    pub fn next_to_draft(&self) -> Result<Option<Task>> {
        let all = self.all()?;
        let busy: Vec<&str> = all
            .iter()
            .filter(|t| t.state.holds_the_repo())
            .map(|t| t.repo.as_str())
            .collect();
        Ok(all
            .iter()
            .find(|t| t.state == TaskState::Queued && !busy.contains(&t.repo.as_str()))
            .cloned())
    }

    /// Is anything currently drafting for `repo`?
    pub fn repo_busy(&self, repo: &str) -> Result<bool> {
        Ok(self
            .all()?
            .iter()
            .any(|t| t.repo == repo && t.state.holds_the_repo()))
    }

    /// Move a task to `state`, recording why.
    pub fn set_state(&self, id: &str, state: TaskState, note: Option<String>) -> Result<Task> {
        let mut task = self.require(id)?;
        task.set_state(state, note);
        self.put(&task)?;
        Ok(task)
    }

    /// Forget a task entirely. Used by tests and by a developer clearing out
    /// settled records; the ordinary way to drop a task is `Discarded`, which
    /// keeps it visible.
    pub fn remove(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.path_for(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    fn queue(tag: &str) -> (Queue, PathBuf) {
        let dir = temp_dir(tag);
        (Queue::open(&dir).unwrap(), dir)
    }

    fn file(q: &Queue, id: &str, repo: &str) -> Task {
        let t = Task::new(id, format!("task {id}"), repo);
        q.put(&t).unwrap();
        t
    }

    #[test]
    fn a_filed_task_survives_being_reopened() {
        // The durability claim: the queue is on disk, so a daemon restart (or a
        // power cut) does not lose a task filed from a train.
        let (q, dir) = queue("durable");
        file(&q, "0001", "alpha");
        drop(q);

        let reopened = Queue::open(&dir).unwrap();
        let tasks = reopened.all().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "0001");
        assert_eq!(tasks[0].state, TaskState::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasks_come_back_oldest_first() {
        let (q, dir) = queue("order");
        file(&q, "0003", "alpha");
        file(&q, "0001", "alpha");
        file(&q, "0002", "alpha");

        let ids: Vec<String> = q.all().unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["0001", "0002", "0003"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_next_task_is_the_oldest_queued_one() {
        let (q, dir) = queue("next");
        file(&q, "0001", "alpha");
        file(&q, "0002", "alpha");
        assert_eq!(q.next_to_draft().unwrap().unwrap().id, "0001");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repo_that_is_drafting_is_skipped_but_others_still_run() {
        // Serialised per REPOSITORY, not per runner: work for a free repo must
        // not wait behind work for a busy one.
        let (q, dir) = queue("per-repo");
        file(&q, "0001", "alpha");
        file(&q, "0002", "alpha");
        file(&q, "0003", "beta");
        q.set_state("0001", TaskState::Drafting, None).unwrap();

        let next = q.next_to_draft().unwrap().expect("beta is free");
        assert_eq!(next.id, "0003", "alpha is busy, so beta goes next");
        assert!(q.repo_busy("alpha").unwrap());
        assert!(!q.repo_busy("beta").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_task_awaiting_review_does_not_hold_its_repository() {
        // THE rule that keeps one unread review from starving a repo. If awaiting
        // review held the slot, spec 20's "deferring is free" would be false.
        let (q, dir) = queue("awaiting-frees");
        file(&q, "0001", "alpha");
        file(&q, "0002", "alpha");
        q.set_state("0001", TaskState::AwaitingReview, None)
            .unwrap();

        assert!(!q.repo_busy("alpha").unwrap());
        assert_eq!(
            q.next_to_draft().unwrap().unwrap().id,
            "0002",
            "the next task for alpha proceeds while the first waits for a human"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_queued_means_nothing_to_do() {
        let (q, dir) = queue("empty");
        assert!(q.next_to_draft().unwrap().is_none());
        file(&q, "0001", "alpha");
        q.set_state("0001", TaskState::Ready, None).unwrap();
        assert!(
            q.next_to_draft().unwrap().is_none(),
            "a settled task is not work"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_corrupt_record_does_not_hide_the_rest_of_the_queue() {
        // A developer with one bad file must still see their other tasks — losing
        // the whole queue to one torn write would be the worst outcome.
        let (q, dir) = queue("corrupt");
        file(&q, "0001", "alpha");
        std::fs::write(dir.join("0002.json"), "{ truncated").unwrap();
        file(&q, "0003", "alpha");

        let tasks = q.all().unwrap();
        assert_eq!(tasks.len(), 2, "the readable tasks are still listed");
        // …and the corruption is visible rather than silently absent.
        assert_eq!(q.unreadable().unwrap(), vec!["0002".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_changes_are_persisted_with_their_reason() {
        let (q, dir) = queue("transition");
        file(&q, "0001", "alpha");
        q.set_state("0001", TaskState::Failed, Some("backend down".into()))
            .unwrap();

        let back = q.require("0001").unwrap();
        assert_eq!(back.state, TaskState::Failed);
        assert_eq!(back.note.as_deref(), Some("backend down"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_id_is_a_clear_error_and_a_missing_one_is_none() {
        let (q, dir) = queue("missing");
        assert!(q.get("nope").unwrap().is_none());
        assert!(q.require("nope").unwrap_err().to_string().contains("nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_task_file_in_the_queue_directory_is_ignored() {
        // Editors and backup tools leave things around; none of it is a task.
        let (q, dir) = queue("stray");
        file(&q, "0001", "alpha");
        std::fs::write(dir.join("notes.txt"), "not a task").unwrap();
        std::fs::write(dir.join(".DS_Store"), "").unwrap();
        assert_eq!(q.all().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
