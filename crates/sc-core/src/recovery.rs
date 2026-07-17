//! Loop/stall detection and stop reasons (spec 03 — VERIFY & failure policy).
//!
//! A small model's signature failure is *getting stuck*: repeating the same
//! action, or thrashing without changing anything. The harness — not the model —
//! detects this cheaply every turn (an action hash + a no-progress counter) and
//! decides when to intervene (re-plan, consult the advisor, or stop). This is the
//! machinery; the loop wires it to those responses.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

/// Why the run stopped — a structured outcome the CLI can render honestly
/// (spec 06 — honest stop lines) instead of a bare bool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// The model called `finish` and any whole-suite gate passed.
    Finished,
    /// The step/turn budget was exhausted before finishing.
    BudgetExhausted,
    /// The agent stalled (looping or making no progress) and couldn't recover.
    Stalled(String),
    /// Escalated to the advisor and still could not proceed (or no advisor).
    Escalated(String),
    /// The user cancelled the run (via the GUI Cancel button). The loop stopped at a turn
    /// boundary; any partial edits are handled by the caller (the GUI reverts them).
    Cancelled,
}

impl StopReason {
    pub fn is_finished(&self) -> bool {
        matches!(self, StopReason::Finished)
    }
}

/// Tracks consecutive repeats and no-progress turns to spot a stuck agent.
#[derive(Debug, Clone, Default)]
pub struct StallDetector {
    last_action: Option<u64>,
    repeat_count: usize,
    no_progress_count: usize,
    /// Distinct actions seen since the last real progress (a workspace change). A read of a
    /// file not yet read in this window is *investigation*, not idling — common and correct
    /// when diagnosing a cross-file bug (read app.py, store.py, routes.py, then fix). So a
    /// NEW action resets the no-progress streak; only a NON-novel action (re-read the same
    /// file, re-run verification) increments it. This stops the harness from guillotining a
    /// model mid-investigation (observed live: a 5-file integration pass died after 4 distinct
    /// reads, never reaching the fix) while still catching genuine spinning.
    seen_since_progress: std::collections::HashSet<u64>,
}

/// What the detector recommends after observing a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Healthy — keep going.
    Ok,
    /// The same action has repeated too many times — intervene.
    Looping,
    /// Too many turns with no workspace change — intervene.
    Stuck,
}

impl StallDetector {
    /// Record a turn: the action taken (tool + key args, already hashed by
    /// [`action_hash`]) and whether the workspace changed this turn. `repeat_limit`
    /// and `no_progress_limit` are the thresholds for intervention.
    pub fn observe(
        &mut self,
        action: u64,
        changed_workspace: bool,
        repeat_limit: usize,
        no_progress_limit: usize,
    ) -> Progress {
        // Repeated identical action.
        if self.last_action == Some(action) {
            self.repeat_count += 1;
        } else {
            self.repeat_count = 0;
            self.last_action = Some(action);
        }

        // No-progress streak. A workspace change is unambiguous progress — reset, and clear
        // the investigation window. Otherwise, a NOVEL action (e.g. reading a file not yet
        // read this window) is investigation toward a fix, so it also resets the streak; only
        // a NON-novel action (re-reading the same file, re-running verification) — true
        // idling — increments it. The repeat detector still catches back-to-back duplicates.
        if changed_workspace {
            self.no_progress_count = 0;
            self.seen_since_progress.clear();
        } else if self.seen_since_progress.insert(action) {
            // First time we've seen this action since the last change → investigation.
            self.no_progress_count = 0;
        } else {
            // A read/verify we've already done this window → not advancing.
            self.no_progress_count += 1;
        }

        if self.repeat_count + 1 >= repeat_limit {
            Progress::Looping
        } else if self.no_progress_count >= no_progress_limit {
            Progress::Stuck
        } else {
            Progress::Ok
        }
    }

    /// Reset after an intervention (re-plan / advice) so the agent gets a fresh
    /// run at making progress before we intervene again.
    pub fn reset(&mut self) {
        self.last_action = None;
        self.repeat_count = 0;
        self.no_progress_count = 0;
        self.seen_since_progress.clear();
    }
}

/// Hash a (tool, key-args) action so repeats are detectable regardless of any
/// surrounding prose the model emitted.
pub fn action_hash(tool: &str, args: &str) -> u64 {
    let mut h = DefaultHasher::new();
    tool.hash(&mut h);
    args.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_repeated_identical_actions() {
        let mut d = StallDetector::default();
        let a = action_hash("read_file", "a.rs");
        // repeat_limit = 3 -> the 3rd identical action trips it.
        assert_eq!(d.observe(a, true, 3, 5), Progress::Ok);
        assert_eq!(d.observe(a, true, 3, 5), Progress::Ok);
        assert_eq!(d.observe(a, true, 3, 5), Progress::Looping);
    }

    #[test]
    fn a_different_action_resets_the_repeat_streak() {
        let mut d = StallDetector::default();
        let a = action_hash("read_file", "a.rs");
        let b = action_hash("read_file", "b.rs");
        d.observe(a, true, 3, 5);
        d.observe(a, true, 3, 5);
        assert_eq!(d.observe(b, true, 3, 5), Progress::Ok); // streak broken
    }

    #[test]
    fn distinct_reads_are_investigation_not_a_stall() {
        // Reading several DIFFERENT files without writing is how a model diagnoses a
        // cross-file bug — each novel read is progress, so it must NOT trip the no-progress
        // stall (no_progress_limit=3 here). (Before: this guillotined the integration pass
        // mid-investigation.)
        let mut d = StallDetector::default();
        for i in 0..6 {
            let a = action_hash("read_file", &format!("f{i}.rs"));
            assert_eq!(
                d.observe(a, false, 8, 3),
                Progress::Ok,
                "distinct read #{i} should be progress, not a stall"
            );
        }
    }

    #[test]
    fn re_reading_the_same_files_still_stalls() {
        // The protection that remains: once there's nothing NEW to read, re-reading files
        // already seen this window is idling and must trip the no-progress stall. Use two
        // files alternating so the *repeat* detector (back-to-back identical) doesn't fire
        // first — this isolates the no-progress path.
        let mut d = StallDetector::default();
        let a = action_hash("read_file", "a.rs");
        let b = action_hash("read_file", "b.rs");
        // Novel reads: progress.
        assert_eq!(d.observe(a, false, 8, 3), Progress::Ok);
        assert_eq!(d.observe(b, false, 8, 3), Progress::Ok);
        // Now re-reading the same two (non-novel) → no-progress climbs to the limit.
        assert_eq!(d.observe(a, false, 8, 3), Progress::Ok); // count 1
        assert_eq!(d.observe(b, false, 8, 3), Progress::Ok); // count 2
        assert_eq!(d.observe(a, false, 8, 3), Progress::Stuck); // count 3 → stuck
    }

    #[test]
    fn workspace_change_resets_no_progress() {
        let mut d = StallDetector::default();
        let a = action_hash("edit_file", "a.rs");
        d.observe(a, false, 5, 2);
        let b = action_hash("edit_file", "b.rs");
        assert_eq!(d.observe(b, true, 5, 2), Progress::Ok); // change resets
    }

    #[test]
    fn reset_clears_state() {
        let mut d = StallDetector::default();
        let a = action_hash("x", "y");
        d.observe(a, false, 3, 3);
        d.observe(a, false, 3, 3);
        d.reset();
        // After reset, a single observe can't immediately re-trip.
        assert_eq!(d.observe(a, false, 3, 3), Progress::Ok);
    }
}
