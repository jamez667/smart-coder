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
    /// How many times this state has been written, for compare-and-swap
    /// (spec 19 — the single-writer problem).
    ///
    /// [`save_to`] refuses when the copy on disk has moved on from the one we
    /// loaded, so a stale writer fails loudly instead of silently overwriting a
    /// decision someone else made. The [`crate::lease`] is the first line of
    /// defence and this is the second: a lease can be reclaimed after an expiry,
    /// and a process that was merely paused must not then clobber its successor.
    ///
    /// `serde(default)` so every `state.json` written before this existed loads
    /// as generation 0 — no migration, no breakage.
    #[serde(default)]
    generation: u64,
}

impl WorkflowState {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            artifacts: Vec::new(),
            feedback: Vec::new(),
            generation: 0,
        }
    }

    /// How many times this state has been persisted.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Continue from a generation loaded off disk.
    ///
    /// A resuming run rebuilds its state from the artifacts it adopted rather than
    /// deserializing wholesale, so it starts at generation 0 and would look like a
    /// stale writer to its own predecessor. Adopting the number says "this is that
    /// state, continued" — which is only sound because the caller holds the lease
    /// on the directory. Never call this to force a save past a genuine conflict.
    pub fn adopt_generation(&mut self, generation: u64) {
        self.generation = generation;
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

    /// Whether every phase has an approved artifact.
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
pub fn save(workspace: &Path, state: &mut WorkflowState) -> Result<()> {
    save_to(&plan_dir(workspace), state, false)
}

/// Persist to an explicit `dir` with a choice of filename style: numbered (`NN-phase.md`, the
/// default plan-dir layout) or OpenSpec (`spec.md`/`architecture.md`/… — for the `specs/<slug>/`
/// layout). `state.json` is always written so a run can resume.
///
/// **Compare-and-swap** (spec 19): fails if the `state.json` on disk has a higher
/// generation than the one we are writing — someone else wrote while we held this
/// copy, and overwriting would silently discard their decision. That is exactly
/// the failure spec 19 describes: an approval made on one surface, clobbered by
/// another surface's next phase save, with nothing logged.
///
/// Takes `&mut` because a successful save bumps the generation. That is the point:
/// the counter is only meaningful if it advances with the writes.
pub fn save_to(dir: &Path, state: &mut WorkflowState, openspec_names: bool) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    // Compare before writing anything, so a refused save leaves the directory
    // untouched rather than half-updated.
    if let Some(on_disk) = load_from(dir)? {
        if on_disk.generation > state.generation {
            return Err(sc_proto::DcError::Eval(format!(
                "{} changed underneath this run (on disk: generation {}, ours: {}). \
                 Another process wrote it — reload before saving so their work is not lost.",
                dir.join("state.json").display(),
                on_disk.generation,
                state.generation,
            )));
        }
    }

    state.generation += 1;
    for a in &state.artifacts {
        let name = if openspec_names {
            a.phase.openspec_filename().to_string()
        } else {
            a.phase.filename()
        };
        write_atomic(&dir.join(name), a.content.as_bytes())?;
    }
    let json =
        serde_json::to_string_pretty(state).map_err(|e| sc_proto::DcError::Eval(e.to_string()))?;
    write_atomic(&dir.join("state.json"), json.as_bytes())?;
    Ok(())
}

/// Write `bytes` to `path` so a reader never sees a partial file.
///
/// Write a sibling temp file, flush it to disk, then rename over the target —
/// rename is atomic on NTFS and POSIX alike. A plain `fs::write` truncates first,
/// so a crash (or a full disk) mid-write leaves a truncated `state.json` and the
/// run's whole history is gone. The phase `.md` artifacts get the same treatment:
/// a half-written spec is as bad as a half-written state.
///
/// The temp name carries the pid so two processes writing the same directory
/// cannot collide on it — which the lease should prevent, but a safety net that
/// costs one `format!` is worth having.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // Without this the rename can land before the contents reach disk, and a
        // power cut leaves an intact-looking file full of zeroes.
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave the temp behind to be mistaken for an artifact.
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Load a previously-saved workflow from the default plan dir, if `state.json` exists.
pub fn load(workspace: &Path) -> Result<Option<WorkflowState>> {
    load_from(&plan_dir(workspace))
}

/// Load a previously-saved workflow from an explicit `dir` (the artifact dir a run wrote to —
/// `.smart-coder/plan/` or `specs/<slug>/`), if its `state.json` exists. `None` when there's no
/// saved state there. This is what lets a **Build** reuse the breakdown a prior **Breakdown** run
/// approved: same task → same artifact dir → its approved `state.json` is adopted instead of
/// regenerating the design phases.
pub fn load_from(dir: &Path) -> Result<Option<WorkflowState>> {
    let path = dir.join("state.json");
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
#[path = "state_tests.rs"]
mod tests;
