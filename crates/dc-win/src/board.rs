//! A live per-subtask board folded from the [`SwarmEvent`] stream (no iced types, so
//! it's host-testable). A swarm runs several workers concurrently and they all emit
//! into one flat stream; this collapses that stream into one row per subtask with its
//! *current* status, so the UI can show "what each coder is doing" at a glance instead
//! of an interleaved log.
//!
//! Every swarm event carries `subtask` as its identity key, so the fold is a simple
//! state machine keyed by that string, preserving first-seen order.

use dc_swarm::SwarmEvent;

/// Where a subtask is in its lifecycle. Ordered roughly by progression; `Retrying`
/// and the terminal accept/reject states are set as events arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtaskStatus {
    /// A worker is running it.
    Running,
    /// The worker finished; awaiting integration.
    Finished,
    /// Re-dispatched after an incomplete/rejected integration: `attempt`/`max` and
    /// the count of still-red scoped tests.
    Retrying {
        attempt: usize,
        max: usize,
        red: usize,
    },
    /// The advisor was consulted before the final retry ("junior asks senior").
    AskedSenior,
    /// Integrated successfully; `files` are the changed paths (empty ⇒ no changes).
    Integrated { files: Vec<String> },
    /// The proposal was rejected/reverted.
    Reverted,
}

impl SubtaskStatus {
    /// A short status glyph for the row.
    pub fn icon(&self) -> &'static str {
        match self {
            SubtaskStatus::Running => "▸",
            SubtaskStatus::Finished => "◇",
            SubtaskStatus::Retrying { .. } => "↻",
            SubtaskStatus::AskedSenior => "⚑",
            SubtaskStatus::Integrated { .. } => "✓",
            SubtaskStatus::Reverted => "✗",
        }
    }

    /// Whether this status should read as a problem (for colouring).
    pub fn is_bad(&self) -> bool {
        matches!(
            self,
            SubtaskStatus::Retrying { .. } | SubtaskStatus::Reverted
        )
    }
}

/// One board row: the subtask id/goal, its current status, and the worker's proposed
/// file content (once it has finished) so the UI can show *what* the coder produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRow {
    pub subtask: String,
    pub goal: String,
    pub status: SubtaskStatus,
    /// The worker's full proposed content, set when it finishes. `None` until then.
    pub proposal: Option<String>,
}

impl BoardRow {
    /// A compact one-line description for rendering: `"<id>  <goal-or-status-detail>"`.
    pub fn detail(&self) -> String {
        match &self.status {
            SubtaskStatus::Retrying { attempt, max, red } => {
                let s = if *red == 1 { "" } else { "s" };
                format!("retry {attempt}/{max} — {red} test{s} red")
            }
            SubtaskStatus::Integrated { files } if !files.is_empty() => {
                format!("integrated — {}", files.join(", "))
            }
            SubtaskStatus::Integrated { .. } => "integrated — (no file changes)".to_string(),
            SubtaskStatus::Reverted => "reverted".to_string(),
            _ => self.goal.clone(),
        }
    }
}

/// The live board: per-subtask status in first-seen order.
#[derive(Debug, Default, Clone)]
pub struct SwarmBoard {
    rows: Vec<BoardRow>,
}

impl SwarmBoard {
    /// Fold one [`SwarmEvent`] into the board.
    pub fn apply(&mut self, ev: &SwarmEvent) {
        use SwarmEvent::*;
        match ev {
            Decomposed { .. } => {
                // The `Decomposed` event carries only goal *strings*, not the stable
                // subtask ids the later events key on (the orchestrator assigns ids
                // like `t1`, or model-provided/merged ids). Seeding rows from it would
                // guess ids that don't match `WorkerStarted`, producing duplicate or
                // orphan rows. So the board is driven entirely by the id-bearing
                // events from `WorkerStarted` onward — authoritative, no guessing. The
                // flat activity stream still shows the decomposition itself.
            }
            OrchestratorPrompt { .. } => {}
            WorkerStarted { subtask, goal, .. } => {
                self.upsert(subtask, goal, SubtaskStatus::Running);
            }
            WorkerFinished {
                subtask, proposal, ..
            } => {
                self.set_status(subtask, SubtaskStatus::Finished);
                // Stash what the worker actually produced, for the UI to expand.
                if let Some(row) = self.rows.iter_mut().find(|r| r.subtask == *subtask) {
                    row.proposal = Some(proposal.clone());
                }
            }
            SubtaskRetry {
                subtask,
                attempt,
                max,
                failing_tests,
            } => {
                self.set_status(
                    subtask,
                    SubtaskStatus::Retrying {
                        attempt: *attempt,
                        max: *max,
                        red: failing_tests.len(),
                    },
                );
            }
            AdvisorConsulted { subtask, .. } => {
                self.set_status(subtask, SubtaskStatus::AskedSenior);
            }
            Integrated {
                subtask,
                accepted,
                files,
            } => {
                let status = if *accepted {
                    SubtaskStatus::Integrated {
                        files: files.clone(),
                    }
                } else {
                    SubtaskStatus::Reverted
                };
                self.set_status(subtask, status);
            }
            SwarmDone { .. } => {}
        }
    }

    /// The rows in first-seen order.
    pub fn rows(&self) -> &[BoardRow] {
        &self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Insert or update a row, keyed by subtask id. If no row with this id exists yet
    /// (the usual case for a `WorkerStarted`), append one; otherwise update its status
    /// and adopt the (non-empty) goal text.
    fn upsert(&mut self, subtask: &str, goal: &str, status: SubtaskStatus) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.subtask == subtask) {
            row.status = status;
            if !goal.is_empty() {
                row.goal = goal.to_string();
            }
        } else {
            self.rows.push(BoardRow {
                subtask: subtask.to_string(),
                goal: goal.to_string(),
                status,
                proposal: None,
            });
        }
    }

    /// Update only the status of an existing row; if the id is somehow unseen (a
    /// status event arriving before its `WorkerStarted`), create a bare row so the
    /// event isn't lost.
    fn set_status(&mut self, subtask: &str, status: SubtaskStatus) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.subtask == subtask) {
            row.status = status;
        } else {
            self.rows.push(BoardRow {
                subtask: subtask.to_string(),
                goal: String::new(),
                status,
                proposal: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: &str, goal: &str) -> SwarmEvent {
        SwarmEvent::WorkerStarted {
            subtask: id.to_string(),
            goal: goal.to_string(),
            prompt: format!("Task: {goal}"),
        }
    }

    #[test]
    fn worker_lifecycle_collapses_to_one_row() {
        let mut b = SwarmBoard::default();
        b.apply(&started("T1", "build the parser"));
        assert_eq!(b.rows().len(), 1);
        assert_eq!(b.rows()[0].status, SubtaskStatus::Running);

        b.apply(&SwarmEvent::WorkerFinished {
            subtask: "T1".to_string(),
            summary: "done".to_string(),
            proposal: "fn main() { /* the fix */ }".to_string(),
        });
        assert_eq!(b.rows().len(), 1, "same subtask updates in place");
        assert_eq!(b.rows()[0].status, SubtaskStatus::Finished);
        assert_eq!(
            b.rows()[0].proposal.as_deref(),
            Some("fn main() { /* the fix */ }"),
            "the worker's proposed content is stashed on finish"
        );

        b.apply(&SwarmEvent::Integrated {
            subtask: "T1".to_string(),
            accepted: true,
            files: vec!["src/parser.rs".to_string()],
        });
        assert!(matches!(
            b.rows()[0].status,
            SubtaskStatus::Integrated { .. }
        ));
        assert!(b.rows()[0].detail().contains("src/parser.rs"));
    }

    #[test]
    fn concurrent_workers_get_distinct_rows_in_first_seen_order() {
        let mut b = SwarmBoard::default();
        b.apply(&started("T1", "a"));
        b.apply(&started("T2", "b"));
        b.apply(&SwarmEvent::WorkerFinished {
            subtask: "T1".to_string(),
            summary: "x".to_string(),
            proposal: String::new(),
        });
        // Two distinct rows; T1 advanced, T2 still running; order preserved.
        assert_eq!(b.rows().len(), 2);
        assert_eq!(b.rows()[0].subtask, "T1");
        assert_eq!(b.rows()[0].status, SubtaskStatus::Finished);
        assert_eq!(b.rows()[1].subtask, "T2");
        assert_eq!(b.rows()[1].status, SubtaskStatus::Running);
    }

    #[test]
    fn retry_and_revert_read_as_bad() {
        let mut b = SwarmBoard::default();
        b.apply(&started("T1", "a"));
        b.apply(&SwarmEvent::SubtaskRetry {
            subtask: "T1".to_string(),
            attempt: 1,
            max: 2,
            failing_tests: vec!["t1".to_string(), "t2".to_string()],
        });
        assert!(b.rows()[0].status.is_bad());
        assert!(b.rows()[0].detail().contains("2 tests red"));

        b.apply(&SwarmEvent::Integrated {
            subtask: "T1".to_string(),
            accepted: false,
            files: vec!["weakened a contract test".to_string()],
        });
        assert_eq!(b.rows()[0].status, SubtaskStatus::Reverted);
        assert!(b.rows()[0].status.is_bad());
    }

    #[test]
    fn decomposed_does_not_seed_rows_only_id_bearing_events_do() {
        // `Decomposed` carries only goal strings (no stable ids), so it must NOT add
        // board rows — otherwise it would guess ids that don't match `WorkerStarted`
        // (real ids look like `t1`), producing duplicate/orphan rows. The board fills
        // in from the first id-bearing event.
        let mut b = SwarmBoard::default();
        b.apply(&SwarmEvent::Decomposed {
            subtasks: vec!["goal one".to_string(), "goal two".to_string()],
        });
        assert!(b.is_empty(), "Decomposed alone seeds nothing");

        b.apply(&started("t1", "goal one"));
        assert_eq!(b.rows().len(), 1);
        assert_eq!(b.rows()[0].subtask, "t1");
        assert_eq!(b.rows()[0].status, SubtaskStatus::Running);
    }
}
