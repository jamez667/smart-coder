//! Workflow state: the chain of phase artifacts, durable on disk (spec 09).
//!
//! Artifacts are the state — not anything held in a model's context. They're
//! written under `<workspace>/.smart-coder/plan/`, one Markdown file per phase, so
//! the plan is reviewable as a diff and the workflow is resumable across sessions.

use std::path::{Path, PathBuf};

use sc_proto::Result;
use serde::{Deserialize, Serialize};

use crate::phase::Phase;

/// Where a phase artifact stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Produced by the model, not yet accepted.
    Draft,
    /// Accepted at its checkpoint — frozen grounding for later phases.
    Approved,
}

/// One phase's produced document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub phase: Phase,
    pub content: String,
    pub status: Status,
}

impl Artifact {
    pub fn draft(phase: Phase, content: impl Into<String>) -> Self {
        Self {
            phase,
            content: content.into(),
            status: Status::Draft,
        }
    }

    pub fn is_approved(&self) -> bool {
        self.status == Status::Approved
    }
}

/// The full workflow: the original task plus the artifact chain produced so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub task: String,
    /// Artifacts in pipeline order (at most one per phase).
    artifacts: Vec<Artifact>,
    /// Send-back feedback notes keyed by phase: when a checkpoint bounces back to a
    /// phase with notes, they're stored here so the *next* regeneration of that
    /// phase can ground on them (spec 09 — "return … with feedback notes"). Cleared
    /// once that phase is approved again.
    #[serde(default)]
    feedback: Vec<(Phase, String)>,
}

impl WorkflowState {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            artifacts: Vec::new(),
            feedback: Vec::new(),
        }
    }

    /// The artifact for `phase`, if produced.
    pub fn artifact(&self, phase: Phase) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.phase == phase)
    }

    /// All approved artifacts, in pipeline order — the grounding context handed to
    /// the next phase.
    pub fn approved(&self) -> Vec<&Artifact> {
        let mut v: Vec<&Artifact> = self.artifacts.iter().filter(|a| a.is_approved()).collect();
        v.sort_by_key(|a| a.phase.index());
        v
    }

    /// The next phase to produce: the first phase with no artifact yet. `None` when
    /// every phase has an artifact.
    pub fn next_phase(&self) -> Option<Phase> {
        Phase::ALL
            .iter()
            .copied()
            .find(|p| self.artifact(*p).is_none())
    }

    /// Insert or replace `phase`'s artifact.
    pub fn set(&mut self, artifact: Artifact) {
        if let Some(slot) = self
            .artifacts
            .iter_mut()
            .find(|a| a.phase == artifact.phase)
        {
            *slot = artifact;
        } else {
            self.artifacts.push(artifact);
            self.artifacts.sort_by_key(|a| a.phase.index());
        }
    }

    /// Approve `phase`'s draft (no-op if absent). Clears any send-back feedback for
    /// the phase — once it's approved, the note has served its purpose.
    pub fn approve(&mut self, phase: Phase) {
        if let Some(a) = self.artifacts.iter_mut().find(|a| a.phase == phase) {
            a.status = Status::Approved;
        }
        self.feedback.retain(|(p, _)| *p != phase);
    }

    /// Record send-back feedback for `phase` — grounding for its next regeneration
    /// (spec 09). Replaces any prior note for the phase.
    pub fn set_feedback(&mut self, phase: Phase, notes: impl Into<String>) {
        self.feedback.retain(|(p, _)| *p != phase);
        self.feedback.push((phase, notes.into()));
    }

    /// The send-back feedback recorded for `phase`, if any.
    pub fn feedback(&self, phase: Phase) -> Option<&str> {
        self.feedback
            .iter()
            .find(|(p, _)| *p == phase)
            .map(|(_, n)| n.as_str())
    }

    /// Drop every artifact at or after `phase` — used when sending back to an
    /// earlier phase, since downstream work was grounded on what we're changing
    /// (spec 09: send-back invalidates and regenerates downstream).
    pub fn invalidate_from(&mut self, phase: Phase) {
        self.artifacts.retain(|a| a.phase.index() < phase.index());
    }

    /// Whether all six phases have an approved artifact.
    pub fn is_complete(&self) -> bool {
        Phase::ALL
            .iter()
            .all(|p| self.artifact(*p).is_some_and(Artifact::is_approved))
    }
}

/// The plan directory under a workspace.
pub fn plan_dir(workspace: &Path) -> PathBuf {
    workspace.join(".smart-coder").join("plan")
}

/// Persist every artifact to `<workspace>/.smart-coder/plan/NN-phase.md` and the
/// task + statuses to `state.json`, so the plan is a reviewable diff and the run
/// resumes from disk.
pub fn save(workspace: &Path, state: &WorkflowState) -> Result<()> {
    let dir = plan_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    for a in &state.artifacts {
        std::fs::write(dir.join(a.phase.filename()), &a.content)?;
    }
    let json =
        serde_json::to_string_pretty(state).map_err(|e| sc_proto::DcError::Eval(e.to_string()))?;
    std::fs::write(dir.join("state.json"), json)?;
    Ok(())
}

/// Load a previously-saved workflow, if `state.json` exists.
pub fn load(workspace: &Path) -> Result<Option<WorkflowState>> {
    let path = plan_dir(workspace).join("state.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let state =
                serde_json::from_str(&s).map_err(|e| sc_proto::DcError::Eval(e.to_string()))?;
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("dc-wf-{tag}-{n}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn next_phase_walks_the_pipeline() {
        let mut s = WorkflowState::new("do a thing");
        assert_eq!(s.next_phase(), Some(Phase::Specs));
        s.set(Artifact::draft(Phase::Specs, "spec body"));
        assert_eq!(s.next_phase(), Some(Phase::Architecture));
    }

    #[test]
    fn approved_returns_only_approved_in_order() {
        let mut s = WorkflowState::new("t");
        s.set(Artifact::draft(Phase::Specs, "s"));
        s.set(Artifact::draft(Phase::Architecture, "a"));
        s.approve(Phase::Specs);
        let approved = s.approved();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].phase, Phase::Specs);
    }

    #[test]
    fn invalidate_from_drops_at_and_after() {
        let mut s = WorkflowState::new("t");
        for p in Phase::ALL {
            s.set(Artifact::draft(p, "x"));
        }
        s.invalidate_from(Phase::Layout);
        assert!(s.artifact(Phase::Architecture).is_some());
        assert!(s.artifact(Phase::Layout).is_none());
        assert!(s.artifact(Phase::WorkDecomposition).is_none());
    }

    #[test]
    fn is_complete_requires_all_approved() {
        let mut s = WorkflowState::new("t");
        for p in Phase::ALL {
            s.set(Artifact::draft(p, "x"));
            s.approve(p);
        }
        assert!(s.is_complete());
        // A single un-approved phase breaks completion.
        s.set(Artifact::draft(Phase::Layout, "redo"));
        assert!(!s.is_complete());
    }

    #[test]
    fn save_then_load_round_trips_and_writes_markdown() {
        let ws = temp("persist");
        let mut s = WorkflowState::new("build a parser");
        s.set(Artifact::draft(Phase::Specs, "# Specs\nbuild it"));
        s.approve(Phase::Specs);
        save(&ws, &s).unwrap();

        // The per-phase Markdown is on disk and reviewable.
        let md = std::fs::read_to_string(plan_dir(&ws).join("01-specs.md")).unwrap();
        assert!(md.contains("build it"));

        let loaded = load(&ws).unwrap().unwrap();
        assert_eq!(loaded, s);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn feedback_is_set_persisted_and_cleared_on_approve() {
        let ws = temp("feedback");
        let mut s = WorkflowState::new("t");
        s.set(Artifact::draft(Phase::Architecture, "draft"));
        s.set_feedback(Phase::Architecture, "make it event-driven");
        assert_eq!(
            s.feedback(Phase::Architecture),
            Some("make it event-driven")
        );

        // Survives a save/load round-trip.
        save(&ws, &s).unwrap();
        let loaded = load(&ws).unwrap().unwrap();
        assert_eq!(
            loaded.feedback(Phase::Architecture),
            Some("make it event-driven")
        );

        // Approving the phase clears its feedback — the note has done its job.
        s.approve(Phase::Architecture);
        assert_eq!(s.feedback(Phase::Architecture), None);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn load_missing_is_none() {
        let ws = temp("missing");
        assert!(load(&ws).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
