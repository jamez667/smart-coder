//! Claim a task, draft its spec, park.
//!
//! ## Specs only, structurally
//!
//! [`spec_only_mode`] is the **one** [`WorkflowMode`] this crate ever constructs,
//! and it stops after the specs phase. Architecture, layout, stage breakdown,
//! decomposition and the staged-build path are therefore *unreachable* from the
//! daemon — not declined, not gated, not behind a flag. Spec 19 insists the "no
//! writing code" anti-goal be **structural, not a policy line**, precisely because
//! a staged-build path already exists that writes across the whole tree with no
//! snapshot and no revert bookkeeping.
//!
//! This is also why the public surface can be exposed at all: its blast radius is
//! one Markdown file, in a repository the developer nominated.
//!
//! ## What a run does
//!
//! ```text
//!   claim  ──► preflight ──► draft the spec ──► park (AwaitingReview)
//!     │            │                                   │
//!     │            └── refused ──► back to Queued       └── error ──► Failed
//! ```
//!
//! Every step that can refuse records *why* on the task, because a run that
//! happened at 3am is read hours later with no other context.

use std::path::Path;

use sc_model::ModelBackend;
use sc_proto::{DcError, Result};
use sc_workflow::{Ceremony, Phase, PhaseSet, WorkflowMode};

use crate::config::DaemonConfig;
use crate::park::ParkingGate;
use crate::preflight;
use crate::queue::Queue;
use crate::task::{Task, TaskState};

/// The only workflow mode the daemon builds: draft the spec, then stop.
///
/// `skip_tests` because tests belong to a contract this daemon never reaches, and
/// `stop_after: Specs` because Phase 1 is the entire product of a drafting run.
pub fn spec_only_mode() -> WorkflowMode {
    WorkflowMode {
        skip_tests: true,
        stop_after: Some(Phase::Specs),
    }
}

/// The phases a drafting run gates at.
///
/// Always exactly the specs phase, whatever ceremony the task carried: it is the
/// only phase that runs, and it must not self-approve. A task filed with
/// `minimal` ceremony still parks — spec 19's first anti-goal is that ceremony
/// never chooses to skip a human at a gate that exists.
pub fn spec_gate_set() -> PhaseSet {
    PhaseSet::of([Phase::Specs])
}

/// What one drafting attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drafted {
    /// A spec is on disk and a human is needed. The task is `AwaitingReview`.
    AwaitingReview { artifact_dir: String },
    /// The repository was not in a state to write into. The task stays `Queued`
    /// so it runs when the developer has finished what they were doing.
    Deferred { reason: String },
    /// **This machine** cannot do it, but another might. Handed back to the
    /// server undone rather than reported as a failure.
    ///
    /// Separate from [`Failed`](Drafted::Failed) because the two say different
    /// things to whoever filed the request: a failure means "this could not be
    /// specified, look at it", where a release means "this machine is the wrong
    /// one". With several daemons serving different repositories, collapsing
    /// them destroys work that another machine could have done.
    ///
    /// Rare now that the server hands over only declared repositories, but not
    /// unreachable: the configuration can change between the poll and the draft,
    /// and a path that is not a git repository is only discovered here.
    Released { reason: String },
    /// The run could not continue. Kept visible and never retried silently.
    Failed { reason: String },
}

/// Draft the spec for one task.
///
/// `orchestrator` produces the artifact; no worker backend is needed, because a
/// drafting run writes no tests and builds nothing.
pub fn draft(
    orchestrator: &dyn ModelBackend,
    queue: &Queue,
    cfg: &DaemonConfig,
    task: &Task,
) -> Result<Drafted> {
    // 0. Feedback never reaches a model. It is a note, not a request, and it is
    //    kept in the daemon's own store rather than drafted into a repository —
    //    see `crate::feedback`. Reaching here at all means something enqueued it
    //    as a task, which is a caller bug worth surfacing rather than quietly
    //    drafting a spec nobody asked for.
    if !task.kind.drafts_a_spec() {
        let reason = format!(
            "{} is {} — that is kept as a note, not drafted into a spec",
            task.id, task.kind
        );
        queue.set_state(&task.id, TaskState::Failed, Some(reason.clone()))?;
        return Ok(Drafted::Failed { reason });
    }

    // 1. Resolve the repository by NAME against the configured set. A request
    //    never carried a path, so there is none to validate here (spec 18).
    let repo = match cfg.repo(&task.repo) {
        Some(r) => r.path.clone(),
        None => {
            // Handed back, not failed. This says nothing about the request —
            // only that this machine does not have that repository — and with
            // several daemons the one that does may be polling right now.
            //
            // The local task returns to `Queued` rather than `Failed`, so the
            // daemon's own record matches the server's.
            let reason = format!(
                "no repository named {:?} is configured on this daemon",
                task.repo
            );
            queue.set_state(&task.id, TaskState::Queued, Some(reason.clone()))?;
            return Ok(Drafted::Released { reason });
        }
    };

    // 2. Refuse a tree mid-operation, and leave the task QUEUED rather than
    //    failing it — the repository will be fine once the developer finishes,
    //    and a failed task would need requeuing by hand for a transient state.
    if let Err(not_ready) = preflight::check(&repo) {
        let reason = not_ready.reason(&repo);
        queue.set_state(&task.id, TaskState::Queued, Some(reason.clone()))?;
        return Ok(match not_ready {
            // A rebase finishes. Deferring costs one poll interval and the
            // developer never has to requeue anything by hand.
            preflight::NotReady::Interrupted { .. } => Drafted::Deferred { reason },
            // A path that is not a git repository does **not** fix itself — it
            // is a typo in `add-repo`, or a directory that moved. Deferring
            // leaves the request claimed on the server, blocking that repository
            // for a full claim timeout, and then does it again, for ever, on a
            // condition only a human at a keyboard can clear. Hand it back so
            // the queue keeps moving and the request becomes visible.
            preflight::NotReady::NotARepo => Drafted::Released { reason },
        });
    }

    // 3. Claim it. Doing this before the model call is what makes the repo busy
    //    for the duration, so a second task for the same repo waits.
    queue.set_state(&task.id, TaskState::Drafting, None)?;

    let gate = ParkingGate::new(spec_gate_set());
    // The kind shapes the question: a bug spec needs reproduction and
    // expected-versus-actual, a feature spec needs non-goals. One generic prompt
    // cannot ask for both, and a bug filed from a phone would come back as a
    // feature-shaped document (see `crate::intake`).
    //
    // The artifact directory is resolved from the RAW text so the slug stays the
    // request rather than the framing.
    let framed = task.kind.frame(&task.text);
    let (artifact_dir, rel) = sc_workflow::artifact_dirs(&task.text, &repo);
    let outcome = sc_workflow::run_workflow_moded_to(
        orchestrator,
        // A drafting run writes no tests, so the worker is never called. Passing
        // the orchestrator keeps the signature honest rather than inventing a
        // stub whose failure mode nobody would recognise.
        orchestrator,
        &framed,
        &repo,
        sc_workflow::ThinkPolicy::default(),
        spec_only_mode(),
        &|_, _| {},
        &gate,
        artifact_dir.as_deref(),
        true,
        &mut |_, _| {},
    );

    match outcome {
        Ok(_) if gate.parked() => {
            let dir = rel.unwrap_or_default();
            let mut task = queue.require(&task.id)?;
            task.artifact_dir = Some(dir.clone());
            task.set_state(TaskState::AwaitingReview, None);
            queue.put(&task)?;
            Ok(Drafted::AwaitingReview { artifact_dir: dir })
        }
        // The run finished without parking. That should not happen — the specs
        // phase is always gated — so treat it as a fault rather than quietly
        // marking the task ready, which would be a self-approval by accident.
        Ok(_) => {
            let reason = "the drafting run finished without stopping for review — refusing to \
                 treat that as an approval"
                .to_string();
            queue.set_state(&task.id, TaskState::Failed, Some(reason.clone()))?;
            Ok(Drafted::Failed { reason })
        }
        Err(e) => {
            let reason = e.to_string();
            queue.set_state(&task.id, TaskState::Failed, Some(reason.clone()))?;
            Ok(Drafted::Failed { reason })
        }
    }
}

/// Draft the next eligible task, if there is one.
///
/// `None` when nothing is queued, or every queued task's repository is busy.
pub fn draft_next(
    orchestrator: &dyn ModelBackend,
    queue: &Queue,
    cfg: &DaemonConfig,
) -> Result<Option<(Task, Drafted)>> {
    let Some(task) = queue.next_to_draft()? else {
        return Ok(None);
    };
    let outcome = draft(orchestrator, queue, cfg, &task)?;
    Ok(Some((task, outcome)))
}

/// Approve a drafted spec: the developer read it and wants it kept.
///
/// The spec is **already on disk** in the repository — the drafting run wrote it
/// there as a draft so a human could read the real file. Approving marks it
/// approved in `state.json` and moves the task to `Ready`.
///
/// **This starts nothing.** `Ready` means the spec is settled and the developer
/// will build it in their IDE when they choose. Calling it "done" would be the
/// queue lying about what happened.
pub fn approve(queue: &Queue, cfg: &DaemonConfig, id: &str) -> Result<Task> {
    let task = queue.require(id)?;
    if task.state != TaskState::AwaitingReview {
        return Err(DcError::Eval(format!(
            "task {id} is {} — only a task awaiting review can be approved",
            task.state
        )));
    }
    let repo = cfg.require_repo(&task.repo)?;
    let (dir, _) = sc_workflow::artifact_dirs(&task.text, &repo.path);
    let dir = dir
        .ok_or_else(|| DcError::Eval(format!("task {id} has no artifact directory to approve")))?;

    // Mark the artifact approved in the state the repository holds, so any
    // surface that later opens this directory sees a settled spec rather than a
    // draft awaiting someone.
    let mut state = sc_workflow::load_from(&dir)?.ok_or_else(|| {
        DcError::Eval(format!(
            "{} holds no drafted spec to approve",
            dir.display()
        ))
    })?;
    state.approve(Phase::Specs);
    sc_workflow::save_to(&dir, &mut state, true)?;

    queue.set_state(id, TaskState::Ready, None)
}

/// Send a drafted spec back: the developer read it and wants it redrafted.
///
/// The task returns to `Queued` with the note attached, and the next drafting run
/// regenerates the spec grounded on it — the same send-back path an interactive
/// gate uses (spec 09).
pub fn send_back(queue: &Queue, cfg: &DaemonConfig, id: &str, notes: &str) -> Result<Task> {
    let task = queue.require(id)?;
    if task.state != TaskState::AwaitingReview {
        return Err(DcError::Eval(format!(
            "task {id} is {} — only a task awaiting review can be sent back",
            task.state
        )));
    }
    if notes.trim().is_empty() {
        return Err(DcError::Eval(
            "a send-back needs a note saying what to change — otherwise the redraft \
             has nothing to go on and will likely produce the same spec"
                .to_string(),
        ));
    }
    send_back_note(cfg, &task, notes)?;
    queue.set_state(id, TaskState::Queued, Some(format!("sent back: {notes}")))
}

/// Record a send-back note where the *next* drafting run will read it, and drop
/// the rejected draft so that run produces a fresh spec rather than restoring the
/// one the developer just turned down.
///
/// Split out from [`send_back`] because a redraft arriving from the server has no
/// local `AwaitingReview` task to transition — the note still has to land, or the
/// regeneration proceeds as if nobody had said anything.
pub fn send_back_note(cfg: &DaemonConfig, task: &Task, notes: &str) -> Result<()> {
    let repo = cfg.require_repo(&task.repo)?;
    let (dir, _) = sc_workflow::artifact_dirs(&task.text, &repo.path);
    if let Some(dir) = dir {
        if let Ok(Some(mut state)) = sc_workflow::load_from(&dir) {
            state.invalidate_from(Phase::Specs);
            state.set_feedback(Phase::Specs, notes);
            sc_workflow::save_to(&dir, &mut state, true)?;
        }
    }
    Ok(())
}

/// Drop a task before it was approved.
pub fn discard(queue: &Queue, id: &str, why: Option<&str>) -> Result<Task> {
    let task = queue.require(id)?;
    if task.state.is_terminal() {
        return Err(DcError::Eval(format!(
            "task {id} is already {}",
            task.state
        )));
    }
    queue.set_state(id, TaskState::Discarded, why.map(str::to_string))
}

/// Where a task's spec lives, if it has one.
pub fn spec_path(cfg: &DaemonConfig, task: &Task) -> Option<std::path::PathBuf> {
    let repo = cfg.repo(&task.repo)?;
    let (dir, _) = sc_workflow::artifact_dirs(&task.text, &repo.path);
    dir.map(|d| d.join(Phase::Specs.openspec_filename()))
}

/// Read a task's drafted spec.
pub fn read_spec(cfg: &DaemonConfig, task: &Task) -> Option<String> {
    std::fs::read_to_string(spec_path(cfg, task)?).ok()
}

/// The repository path for a task, for a caller that needs it.
pub fn repo_path<'a>(cfg: &'a DaemonConfig, task: &Task) -> Option<&'a Path> {
    cfg.repo(&task.repo).map(|r| r.path.as_path())
}

/// The ceremony a task carried, defaulted.
///
/// Present for completeness of the record; a drafting run always gates the specs
/// phase regardless, per [`spec_gate_set`].
pub fn ceremony_of(_task: &Task) -> Ceremony {
    Ceremony::Full
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;
    use crate::task::new_id;
    use crate::test_support::{interrupt, temp_dir, temp_repo};
    use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ToolCalling};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// A backend that answers any phase with a fixed body. No live model (spec 11).
    struct Scripted {
        body: String,
        calls: Mutex<usize>,
    }
    impl Scripted {
        fn new(body: &str) -> Self {
            Self {
                body: body.to_string(),
                calls: Mutex::new(0),
            }
        }
    }
    impl ModelBackend for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
            *self.calls.lock().unwrap() += 1;
            Ok(GenerateResponse::new(self.body.clone()))
        }
    }

    /// A daemon serving two scratch repos — never this workspace, since the
    /// daemon must work against any repository.
    fn fixture(tag: &str) -> (Queue, DaemonConfig, PathBuf, PathBuf, PathBuf) {
        let qdir = temp_dir(&format!("{tag}-q"));
        let alpha = temp_repo(&format!("{tag}-alpha"));
        let beta = temp_repo(&format!("{tag}-beta"));
        let mut cfg = DaemonConfig::default();
        cfg.add("alpha", &alpha).unwrap();
        cfg.add("beta", &beta).unwrap();
        (Queue::open(&qdir).unwrap(), cfg, qdir, alpha, beta)
    }

    fn file_task(q: &Queue, text: &str, repo: &str) -> Task {
        let t = Task::new(new_id(), text, repo);
        q.put(&t).unwrap();
        t
    }

    fn cleanup(dirs: &[&PathBuf]) {
        for d in dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn the_daemon_can_only_ever_build_a_spec_only_mode() {
        // THE structural anti-goal. Spec 19 insists "no writing code" must be
        // structural rather than a policy line, because a staged-build path
        // already exists that writes across the whole tree with no snapshot and
        // no revert bookkeeping. The daemon cannot reach it: the one mode it
        // constructs stops after Specs and writes no tests.
        let mode = spec_only_mode();
        assert_eq!(mode.stop_after, Some(Phase::Specs));
        assert!(mode.skip_tests);

        // And it gates the only phase it runs, so it cannot self-approve.
        assert!(spec_gate_set().contains(Phase::Specs));
    }

    #[test]
    fn a_drafted_task_parks_for_review_and_writes_a_spec_into_its_repo() {
        let (q, cfg, qdir, alpha, beta) = fixture("draft-ok");
        let backend = Scripted::new("# Seat types\n\nA spec for crew roles.");
        let task = file_task(&q, "Add seat types for crew roles", "alpha");

        let out = draft(&backend, &q, &cfg, &task).unwrap();
        let dir = match out {
            Drafted::AwaitingReview { artifact_dir } => artifact_dir,
            other => panic!("expected a park, got {other:?}"),
        };

        // Parked, not approved — a human is still required.
        let back = q.require(&task.id).unwrap();
        assert_eq!(back.state, TaskState::AwaitingReview);
        assert_eq!(back.artifact_dir.as_deref(), Some(dir.as_str()));

        // The spec is a real file in the NOMINATED repo, reviewable as a diff.
        let spec = alpha.canonicalize().unwrap().join(&dir).join("spec.md");
        assert!(spec.is_file(), "{} should exist", spec.display());
        assert!(std::fs::read_to_string(&spec)
            .unwrap()
            .contains("crew roles"));

        // And the other repo was not touched.
        assert!(!beta.join("specs").exists(), "beta must be untouched");
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn drafting_never_reaches_a_later_phase() {
        // Only the specs artifact is produced — no architecture.md, no
        // layout.md, no breakdown.md, nothing built.
        let (q, cfg, qdir, alpha, beta) = fixture("spec-only");
        let backend = Scripted::new("# The spec");
        let task = file_task(&q, "Add seat types", "alpha");
        draft(&backend, &q, &cfg, &task).unwrap();

        let dir = alpha
            .canonicalize()
            .unwrap()
            .join(q.require(&task.id).unwrap().artifact_dir.unwrap());
        assert!(dir.join("spec.md").is_file());
        for later in [
            "architecture.md",
            "layout.md",
            "breakdown.md",
            "decomposition.md",
        ] {
            assert!(
                !dir.join(later).exists(),
                "{later} must be unreachable from the daemon"
            );
        }
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn a_repository_mid_rebase_is_deferred_and_stays_queued() {
        // Not failed: the tree will be fine once the developer finishes, and a
        // failed task would need requeuing by hand for a transient state.
        let (q, cfg, qdir, alpha, beta) = fixture("mid-rebase");
        interrupt(&alpha, "rebase-merge");
        let backend = Scripted::new("# never generated");
        let task = file_task(&q, "Add seat types", "alpha");

        let out = draft(&backend, &q, &cfg, &task).unwrap();
        assert!(matches!(out, Drafted::Deferred { .. }), "{out:?}");

        let back = q.require(&task.id).unwrap();
        assert_eq!(back.state, TaskState::Queued, "requeued, not failed");
        assert!(
            back.note.unwrap().contains("rebase"),
            "the reason is recorded"
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            0,
            "no model call was paid for"
        );
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn a_merely_dirty_repository_still_drafts() {
        // Every real repository has uncommitted work; refusing would make the
        // daemon useless in the case it exists for.
        let (q, cfg, qdir, alpha, beta) = fixture("dirty");
        std::fs::write(alpha.join("wip.rs"), "half-finished\n").unwrap();
        let backend = Scripted::new("# The spec");
        let task = file_task(&q, "Add seat types", "alpha");

        let out = draft(&backend, &q, &cfg, &task).unwrap();
        assert!(matches!(out, Drafted::AwaitingReview { .. }), "{out:?}");
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn a_task_naming_an_unconfigured_repo_is_handed_back_without_touching_anything() {
        // **Inverted deliberately.** It used to fail terminally, which with
        // several daemons means the first one to poll destroys work another
        // machine could have done. "This machine does not have that repository"
        // says nothing about the request, so it goes back to the queue.
        //
        // Unchanged, and still the point: nothing was touched and no model was
        // called.
        let (q, cfg, qdir, alpha, beta) = fixture("unknown-repo");
        let backend = Scripted::new("# never");
        let task = file_task(&q, "Add seat types", "gamma");

        let out = draft(&backend, &q, &cfg, &task).unwrap();
        assert!(matches!(out, Drafted::Released { .. }), "{out:?}");
        assert_eq!(q.require(&task.id).unwrap().state, TaskState::Queued);
        assert_eq!(*backend.calls.lock().unwrap(), 0);
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn approving_writes_nothing_new_and_starts_nothing() {
        // Approve settles the spec and moves the task to Ready. It does NOT
        // build: the developer picks it up in their IDE when they choose.
        let (q, cfg, qdir, alpha, beta) = fixture("approve");
        let backend = Scripted::new("# The spec\n\nBody.");
        let task = file_task(&q, "Add seat types", "alpha");
        draft(&backend, &q, &cfg, &task).unwrap();

        let approved = approve(&q, &cfg, &task.id).unwrap();
        assert_eq!(approved.state, TaskState::Ready);

        // The spec is settled on disk — any surface opening this directory sees
        // an approved artifact, not a draft awaiting someone.
        let dir = alpha
            .canonicalize()
            .unwrap()
            .join(approved.artifact_dir.clone().unwrap());
        let state = sc_workflow::load_from(&dir).unwrap().unwrap();
        assert!(state.artifact(Phase::Specs).unwrap().is_approved());

        // Nothing was built.
        for later in ["architecture.md", "breakdown.md"] {
            assert!(!dir.join(later).exists(), "approve must not build");
        }
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn only_a_task_awaiting_review_can_be_approved() {
        // Approving a queued task would sign off a spec that does not exist.
        let (q, cfg, qdir, alpha, beta) = fixture("approve-guard");
        let task = file_task(&q, "Add seat types", "alpha");
        let err = approve(&q, &cfg, &task.id).expect_err("nothing drafted yet");
        assert!(err.to_string().contains("awaiting review"), "{err}");
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn sending_back_requeues_the_task_with_its_note() {
        let (q, cfg, qdir, alpha, beta) = fixture("send-back");
        let backend = Scripted::new("# Too vague");
        let task = file_task(&q, "Add seat types", "alpha");
        draft(&backend, &q, &cfg, &task).unwrap();

        let back = send_back(&q, &cfg, &task.id, "name the actual roles").unwrap();
        assert_eq!(back.state, TaskState::Queued, "it will be redrafted");
        assert!(back.note.unwrap().contains("name the actual roles"));

        // The note is on disk where the regeneration will read it, and the draft
        // is gone so the next run produces a fresh spec rather than restoring
        // the one that was rejected.
        let dir = alpha
            .canonicalize()
            .unwrap()
            .join(task.artifact_dir_or_slug());
        let state = sc_workflow::load_from(&dir).unwrap().unwrap();
        assert_eq!(
            state.feedback(Phase::Specs),
            Some("name the actual roles"),
            "the redraft must see why it was rejected"
        );
        assert!(
            state.artifact(Phase::Specs).is_none(),
            "the rejected draft is dropped, not restored"
        );
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn a_send_back_needs_a_note() {
        // Without one the redraft has nothing to go on and will likely produce
        // the same spec, which reads to the developer as the tool ignoring them.
        let (q, cfg, qdir, alpha, beta) = fixture("send-back-empty");
        let backend = Scripted::new("# The spec");
        let task = file_task(&q, "Add seat types", "alpha");
        draft(&backend, &q, &cfg, &task).unwrap();

        assert!(send_back(&q, &cfg, &task.id, "   ").is_err());
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn the_runner_takes_the_oldest_queued_task_whose_repo_is_free() {
        let (q, cfg, qdir, alpha, beta) = fixture("next");
        let backend = Scripted::new("# The spec");
        let first = file_task(&q, "First task for alpha", "alpha");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _second = file_task(&q, "Second task for beta", "beta");

        let (taken, _) = draft_next(&backend, &q, &cfg).unwrap().expect("work to do");
        assert_eq!(taken.id, first.id, "oldest first");
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn nothing_queued_means_no_work() {
        let (q, cfg, qdir, alpha, beta) = fixture("idle");
        let backend = Scripted::new("# never");
        assert!(draft_next(&backend, &q, &cfg).unwrap().is_none());
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn discarding_drops_a_task_but_keeps_it_visible() {
        let (q, cfg, qdir, alpha, beta) = fixture("discard");
        let _ = &cfg;
        let task = file_task(&q, "Add seat types", "alpha");
        let dropped = discard(&q, &task.id, Some("filed by mistake")).unwrap();
        assert_eq!(dropped.state, TaskState::Discarded);
        assert!(q.get(&task.id).unwrap().is_some(), "still in the record");
        // A settled task cannot be discarded twice.
        assert!(discard(&q, &task.id, None).is_err());
        cleanup(&[&qdir, &alpha, &beta]);
    }

    #[test]
    fn a_backend_failure_fails_the_task_visibly_and_is_never_retried_silently() {
        struct Dead;
        impl ModelBackend for Dead {
            fn name(&self) -> &str {
                "dead"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    max_context_tokens: 8192,
                    tool_calling: ToolCalling::None,
                    on_device: false,
                }
            }
            fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
                Ok(GenerateResponse::new(String::new()))
            }
        }

        let (q, cfg, qdir, alpha, beta) = fixture("dead-backend");
        let task = file_task(&q, "Add seat types", "alpha");
        let out = draft(&Dead, &q, &cfg, &task).unwrap();
        assert!(matches!(out, Drafted::Failed { .. }), "{out:?}");

        let back = q.require(&task.id).unwrap();
        assert_eq!(back.state, TaskState::Failed);
        assert!(back.note.is_some(), "the reason is recorded");
        // Failed stays failed: the next sweep must not pick it back up.
        assert!(q.next_to_draft().unwrap().is_none());
        cleanup(&[&qdir, &alpha, &beta]);
    }
}
