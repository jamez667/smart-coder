//! Runner tests: grounding, the gate decisions, mode behavior, and board parsing.

use super::drive::*;
use super::ground::*;
use super::mode::*;
use crate::gate::{AutoApprove, Decision, Gate};
use crate::phase::Phase;
use crate::policy::ThinkPolicy;
use crate::state::Artifact;
use sc_model::ModelBackend;
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ToolCalling};
use std::sync::Mutex;

#[test]
fn ground_task_injects_the_plan_body_and_the_real_file_survey() {
    // The live regression: without grounding, a phase saw only "implement PLAN-lakes.md"
    // and designed a generic module tree that ignored the plan and the real files. Grounding
    // must put the plan's BODY and the existing source files into the task.
    let ws = temp("ground");
    std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();
    std::fs::create_dir_all(ws.join("gen")).unwrap();
    std::fs::write(ws.join("gen/terrain.rs"), "pub fn gen() {}").unwrap();
    std::fs::write(ws.join("render.rs"), "pub fn draw() {}").unwrap();
    std::fs::write(
        ws.join("PLAN-lakes.md"),
        "## Plan: lakes\n**Approach:** flood-fill basins\n**Files to touch:**\n- gen/terrain.rs",
    )
    .unwrap();

    let grounded = ground_task("Design how to implement PLAN-lakes.md.", &ws);
    // The plan body is present (not just its name).
    assert!(
        grounded.contains("flood-fill basins"),
        "plan body injected: {grounded}"
    );
    assert!(grounded.contains("follow it"), "framed to follow the plan");
    // The real files are surveyed, so the model edits them instead of inventing a layout.
    assert!(grounded.contains("gen/terrain.rs"), "real file surveyed");
    assert!(grounded.contains("render.rs"), "real file surveyed");
    assert!(grounded.contains("ALREADY EXIST"), "survey framing present");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn ground_task_injects_the_real_contents_of_the_plans_touched_files() {
    // The invented-architecture bug: with only a file LIST, the design phases hallucinated
    // an `ElevationField` struct that doesn't exist. Grounding must inject the real file
    // CONTENTS so the design references types that are actually there.
    let ws = temp("ground-contents");
    std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();
    std::fs::create_dir_all(ws.join("gen")).unwrap();
    std::fs::write(
        ws.join("gen/terrain.rs"),
        "pub struct Terrain { seed: u64 }\nimpl Terrain { pub fn elevation(&self) -> f32 { 0.0 } }",
    )
    .unwrap();
    std::fs::write(
        ws.join("PLAN-lakes.md"),
        "## Plan\n**Files to touch:**\n- `gen/terrain.rs` (update: add lakes)",
    )
    .unwrap();

    let g = ground_task("Design PLAN-lakes.md.", &ws);
    assert!(
        g.contains("EXISTING contents of gen/terrain.rs"),
        "real file injected"
    );
    assert!(
        g.contains("pub struct Terrain"),
        "real type shown so the design uses it"
    );
    assert!(g.contains("fn elevation"), "real method shown");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn plan_touched_files_resolves_crate_relative_paths_by_suffix() {
    // The real bug: the plan names `gen/terrain.rs` but the file lives at
    // crates/city/src/gen/terrain.rs — so exact join fails and grounding never fires.
    // Suffix resolution finds the real file.
    let ws = temp("touched");
    std::fs::create_dir_all(ws.join("crates/city/src/gen")).unwrap();
    std::fs::write(ws.join("crates/city/src/gen/terrain.rs"), "x").unwrap();
    std::fs::write(ws.join("crates/city/src/render.rs"), "x").unwrap();
    let body = "- `gen/terrain.rs` (update)\n- render.rs (logic)\n- gen/missing.rs (nope)";
    let mut got = plan_touched_files(body, &ws);
    got.sort();
    assert_eq!(
        got,
        vec![
            "crates/city/src/gen/terrain.rs".to_string(),
            "crates/city/src/render.rs".to_string(),
        ]
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn ground_task_is_a_noop_without_a_plan_or_files() {
    // The Python eval ladder builds from an empty dir with no PLAN reference — grounding
    // must leave that task untouched so the ladder is unaffected.
    let ws = temp("ground-empty");
    let task = "build a counter API";
    assert_eq!(ground_task(task, &ws), task);
    let _ = std::fs::remove_dir_all(&ws);
}

/// A backend that answers each phase by matching its system prompt, and emits a
/// valid subtask array for the decomposition phase.
struct PhaseScripted {
    replies: Mutex<Vec<(&'static str, &'static str)>>,
}
impl ModelBackend for PhaseScripted {
    fn name(&self) -> &str {
        "phase-scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, req: &GenerateRequest) -> sc_proto::Result<GenerateResponse> {
        let instr = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let replies = self.replies.lock().unwrap();
        for (key, body) in replies.iter() {
            if instr.contains(key) {
                return Ok(GenerateResponse {
                    content: body.to_string(),
                });
            }
        }
        Ok(GenerateResponse {
            content: "(nothing)".to_string(),
        })
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("dc-wf-run-{tag}-{n}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn runs_all_phases_and_emits_a_board() {
    let backend = PhaseScripted {
        replies: Mutex::new(vec![
            ("crisp spec", "# Specs\ngoals"),
            ("DESIGN APPROACH", "# Architecture\nshape"),
            ("file-change list", "# Layout\nfiles"),
            (
                "plan the TESTS",
                r#"[{"file":"test_a.py","covers":"a works"}]"#,
            ),
            // The test-writer worker call (system prompt says "You write a runnable unit-test file").
            ("pytest test file", "def test_a():\n    assert a() == 1"),
            ("ordered plan", "# Plan\nsteps"),
            (
                "per source file",
                r#"[{"id":"t1","goal":"do a","files":["a.py"]},{"id":"t2","goal":"do b","files":["b.py"]}]"#,
            ),
        ]),
    };
    let ws = temp("all");
    // This exercises the Python TDD ladder path — mark the workspace Python explicitly now
    // that a bare workspace detects as Unknown (a generic project) rather than Python.
    std::fs::write(ws.join("requirements.txt"), "flask\n").unwrap();
    let seen = std::cell::RefCell::new(Vec::new());
    // Same backend stands in for both orchestrator and worker here.
    let outcome = run_workflow(
        &backend,
        &backend,
        "build it",
        &ws,
        ThinkPolicy::default(),
        &|p, _| seen.borrow_mut().push(p),
    )
    .unwrap();

    // Every phase ran, in order, and the workflow is complete.
    assert_eq!(seen.into_inner(), Phase::ALL.to_vec());
    assert!(outcome.state.is_complete());
    // The decomposition produced a swarm board with two subtasks.
    assert_eq!(outcome.board.len(), 2);
    // A worker wrote the test file from the coverage plan, and it's on disk.
    assert_eq!(outcome.test_files, vec!["test_a.py"]);
    let test_body = std::fs::read_to_string(ws.join("test_a.py")).unwrap();
    assert!(test_body.contains("def test_a"));
    // Artifacts persisted as reviewable Markdown.
    let arch =
        std::fs::read_to_string(crate::state::plan_dir(&ws).join("02-architecture.md")).unwrap();
    assert!(arch.contains("shape"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn plan_only_stops_at_the_stage_breakdown_and_writes_no_tests() {
    // The Execute-plan flow on a Rust project: run specs → architecture → layout → stage
    // breakdown (as an ordered design, not tests), then STOP. No test files, no
    // decomposition, no board — the user reviews before anything is built.
    let backend = PhaseScripted {
        replies: Mutex::new(vec![
            ("crisp spec", "# Specs\ngoals"),
            ("DESIGN APPROACH", "# Architecture\nshape"),
            ("file-change list", "# Layout\nfiles"),
            // Rust stack → the stage-breakdown system prompt says "ordered set of small
            // implementation stages", so key off that instead of "plan the TESTS".
            (
                "ordered set of small implementation stages",
                "# Breakdown\n1. detect basins",
            ),
        ]),
    };
    let ws = temp("plan-only");
    // Make it a Rust project so ProjectStack::detect → Rust (non-TDD breakdown path).
    std::fs::write(ws.join("Cargo.toml"), "[package]\nname=\"city\"").unwrap();
    let seen = std::cell::RefCell::new(Vec::new());
    let outcome = run_workflow_moded(
        &backend,
        &backend,
        "add lakes to the terrain",
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::plan_only(),
        &|p, _| seen.borrow_mut().push(p),
        &AutoApprove,
    )
    .unwrap();

    // Ran exactly the four design phases, stopping at the stage breakdown.
    assert_eq!(
        seen.into_inner(),
        vec![
            Phase::Specs,
            Phase::Architecture,
            Phase::Layout,
            Phase::StageBreakdown
        ]
    );
    // No tests written, no decomposition board.
    assert!(
        outcome.test_files.is_empty(),
        "plan-only writes no frozen tests"
    );
    assert_eq!(
        outcome.board.len(),
        0,
        "no decomposition/build in plan-only"
    );
    assert!(!outcome.aborted);
    // The design artifacts are on disk for review.
    let breakdown =
        std::fs::read_to_string(crate::state::plan_dir(&ws).join("04-stage-breakdown.md")).unwrap();
    assert!(breakdown.contains("detect basins"));
    // The later phases were never generated.
    assert!(outcome.state.artifact(Phase::WorkDecomposition).is_none());
    let _ = std::fs::remove_dir_all(&ws);
}

/// A full scripted backend that answers every phase + the test-writer worker.
fn full_backend() -> PhaseScripted {
    PhaseScripted {
        replies: Mutex::new(vec![
            ("crisp spec", "# Specs\ngoals"),
            ("DESIGN APPROACH", "# Architecture\nshape"),
            ("file-change list", "# Layout\nfiles"),
            (
                "plan the TESTS",
                r#"[{"file":"test_a.py","covers":"a works"}]"#,
            ),
            ("pytest test file", "def test_a():\n    assert a() == 1"),
            ("ordered plan", "# Plan\nsteps"),
            (
                "per source file",
                r#"[{"id":"t1","goal":"do a","files":["a.py"]}]"#,
            ),
        ]),
    }
}

/// A gate that replays a fixed script of decisions, one per checkpoint visit,
/// recording which phases it was asked about.
struct ScriptedGate {
    decisions: Mutex<std::collections::VecDeque<Decision>>,
    seen: Mutex<Vec<Phase>>,
}
impl ScriptedGate {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into()),
            seen: Mutex::new(Vec::new()),
        }
    }
}
impl Gate for ScriptedGate {
    fn decide(&self, phase: Phase, _artifact: &Artifact) -> Decision {
        self.seen.lock().unwrap().push(phase);
        // Default to Approve once the script is exhausted, so a short script
        // just gates the first few phases and lets the rest sail through.
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Decision::Approve)
    }
}

#[test]
fn auto_approve_path_matches_run_workflow() {
    // run_workflow_gated with AutoApprove behaves exactly like run_workflow.
    let backend = full_backend();
    let ws = temp("gated-auto");
    let outcome = run_workflow_gated(
        &backend,
        &backend,
        "build it",
        &ws,
        ThinkPolicy::default(),
        &|_, _| {},
        &AutoApprove,
    )
    .unwrap();
    assert!(outcome.state.is_complete());
    assert!(!outcome.aborted);
    assert_eq!(outcome.board.len(), 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn abort_stops_early_and_keeps_approved_artifacts() {
    let backend = full_backend();
    let ws = temp("gated-abort");
    // Approve specs + architecture, then abort at the layout checkpoint.
    let gate = ScriptedGate::new(vec![Decision::Approve, Decision::Approve, Decision::Abort]);
    let outcome = run_workflow_gated(
        &backend,
        &backend,
        "build it",
        &ws,
        ThinkPolicy::default(),
        &|_, _| {},
        &gate,
    )
    .unwrap();

    assert!(outcome.aborted);
    assert!(!outcome.state.is_complete());
    // The two approved phases survive; the aborted layout draft does not freeze.
    assert!(outcome.state.artifact(Phase::Specs).unwrap().is_approved());
    assert!(outcome
        .state
        .artifact(Phase::Architecture)
        .unwrap()
        .is_approved());
    assert!(!outcome.state.artifact(Phase::Layout).unwrap().is_approved());
    // Never reached decomposition, so no board.
    assert_eq!(outcome.board.len(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn revise_reads_the_edited_file_from_disk() {
    let backend = full_backend();
    let ws = temp("gated-revise");
    // A gate that, on the very first (Specs) checkpoint, edits the on-disk file
    // and asks for a Revise; everything else approves.
    struct EditingGate {
        ws: std::path::PathBuf,
        edited: Mutex<bool>,
    }
    impl Gate for EditingGate {
        fn decide(&self, phase: Phase, _a: &Artifact) -> Decision {
            if phase == Phase::Specs && !*self.edited.lock().unwrap() {
                std::fs::write(
                    crate::state::plan_dir(&self.ws).join(phase.filename()),
                    "# Specs\nHUMAN EDITED",
                )
                .unwrap();
                *self.edited.lock().unwrap() = true;
                return Decision::Revise;
            }
            Decision::Approve
        }
    }
    let gate = EditingGate {
        ws: ws.clone(),
        edited: Mutex::new(false),
    };
    let outcome = run_workflow_gated(
        &backend,
        &backend,
        "build it",
        &ws,
        ThinkPolicy::default(),
        &|_, _| {},
        &gate,
    )
    .unwrap();

    // The approved specs artifact is the human's edited content, not the draft.
    let specs = outcome.state.artifact(Phase::Specs).unwrap();
    assert!(specs.is_approved());
    assert!(specs.content.contains("HUMAN EDITED"));
    assert!(outcome.state.is_complete());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn referenced_spec_is_adopted_verbatim_and_specs_gate_never_fires() {
    // Building from an existing feature spec: the workflow must ADOPT that spec as the
    // approved Specs artifact instead of regenerating one (which drifts and wastes a gate
    // click). So after the run: the Specs artifact equals the spec file body verbatim and is
    // approved, the backend never generated a Specs phase, and the FIRST gated phase the human
    // saw is Architecture — not Specs.
    let backend = full_backend();
    let ws = temp("adopt-spec");
    // A real feature spec on disk, referenced by the task.
    let spec_body = "# Purpose\nA crisp, human-approved spec.\n\n# Requirements\n- R1\n- R2";
    std::fs::create_dir_all(ws.join("specs/counter")).unwrap();
    std::fs::write(ws.join("specs/counter/spec.md"), spec_body).unwrap();

    let gate = ScriptedGate::new(vec![]); // approve everything; just record which phases it sees
    let outcome = run_workflow_gated(
        &backend,
        &backend,
        "implement specs/counter/spec.md",
        &ws,
        ThinkPolicy::default(),
        &|_, _| {},
        &gate,
    )
    .unwrap();

    // The Specs artifact is the real spec body verbatim, and it is approved.
    let specs = outcome.state.artifact(Phase::Specs).unwrap();
    assert!(specs.is_approved(), "adopted spec is pre-approved");
    assert_eq!(
        specs.content, spec_body,
        "Specs artifact equals the referenced spec body verbatim (not regenerated)"
    );
    // The gate never decided the Specs phase — the first phase it saw is Architecture.
    let seen = gate.seen.lock().unwrap().clone();
    assert_eq!(
        seen.first(),
        Some(&Phase::Architecture),
        "first gated phase is Architecture, not Specs; got {seen:?}"
    );
    assert!(
        !seen.contains(&Phase::Specs),
        "Specs gate never fires: {seen:?}"
    );
    // The run still completes the full chain (Specs approved makes is_complete satisfiable).
    assert!(outcome.state.is_complete());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn send_back_regenerates_with_feedback_and_completes() {
    let ws = temp("gated-sendback");
    // Count how many times the architecture phase is generated: a send-back from
    // Layout to Architecture should regenerate it.
    let arch_calls = std::cell::RefCell::new(0u32);
    let saw_feedback = std::cell::RefCell::new(false);
    {
        let backend = full_backend();
        // Gate: approve specs + architecture, then at the Layout checkpoint send
        // back to Architecture with a note. After that, approve everything.
        let gate = ScriptedGate::new(vec![
            Decision::Approve, // specs
            Decision::Approve, // architecture (1st)
            Decision::SendBack {
                target: Phase::Architecture,
                notes: Some("make it event-driven".to_string()),
            }, // layout → bounce
        ]);
        // Wrap the backend's generate to count architecture regenerations and
        // detect the feedback note reaching the prompt. We can't easily wrap the
        // trait object, so instead inspect via a custom backend.
        struct CountingBackend<'a> {
            inner: PhaseScripted,
            arch_calls: &'a std::cell::RefCell<u32>,
            saw_feedback: &'a std::cell::RefCell<bool>,
        }
        impl ModelBackend for CountingBackend<'_> {
            fn name(&self) -> &str {
                "counting"
            }
            fn capabilities(&self) -> Capabilities {
                self.inner.capabilities()
            }
            fn generate(&self, req: &GenerateRequest) -> sc_proto::Result<GenerateResponse> {
                let joined: String = req.messages.iter().map(|m| m.content.clone()).collect();
                if joined.contains("DESIGN APPROACH") {
                    *self.arch_calls.borrow_mut() += 1;
                    if joined.contains("make it event-driven") {
                        *self.saw_feedback.borrow_mut() = true;
                    }
                }
                self.inner.generate(req)
            }
        }
        let counting = CountingBackend {
            inner: backend,
            arch_calls: &arch_calls,
            saw_feedback: &saw_feedback,
        };
        let outcome = run_workflow_gated(
            &counting,
            &counting,
            "build it",
            &ws,
            ThinkPolicy::default(),
            &|_, _| {},
            &gate,
        )
        .unwrap();
        assert!(outcome.state.is_complete());
        assert!(!outcome.aborted);
    }
    // Architecture was generated twice (initial + after send-back), and the
    // feedback note reached the regeneration prompt.
    assert_eq!(*arch_calls.borrow(), 2);
    assert!(*saw_feedback.borrow());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn shared_artifact_dir_writes_openspec_files_and_resumes() {
    // The CLI and the desktop GUI both resolve their artifact dir with
    // `sc_workflow::artifact_dirs`, so the same task lands in the same
    // `specs/<slug>/` for both front-ends. This proves the engine-side path end to
    // end: the OpenSpec filenames are what's written (not the numbered plan-dir
    // layout), and a second run over the same task ADOPTS that approved design
    // instead of regenerating it — the Breakdown→Build resume the GUI relies on and
    // the CLI previously couldn't reach.
    let backend = full_backend();
    let ws = temp("shared-artifact-dir");
    let task = "Add seat types for crew roles";

    let (artifact_dir, rel) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.expect("a named task yields a specs/<slug>/ dir");
    assert_eq!(rel.as_deref(), Some("specs/add-seat-types-for-crew-roles"));

    let outcome = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();
    assert!(outcome.state.is_complete());

    // OpenSpec filenames, in the shared dir — not `.smart-coder/plan/NN-phase.md`.
    assert!(
        dir.join("spec.md").is_file(),
        "spec.md written to specs/<slug>/"
    );
    assert!(
        dir.join("architecture.md").is_file(),
        "architecture.md written"
    );
    assert!(
        !crate::plan_dir(&ws).join("01-specs.md").exists(),
        "nothing written to the numbered plan dir"
    );

    // A second run over the same task adopts the approved design rather than
    // re-generating and re-gating it: an Abort-everything gate would stop a fresh
    // run at phase 1, but here every phase is already approved on disk, so the run
    // completes without the gate ever being consulted.
    let deny = ScriptedGate::new(vec![Decision::Abort]);
    let resumed = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &deny,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();
    assert!(resumed.state.is_complete(), "prior design adopted");
    assert!(!resumed.aborted, "the gate was never reached");
    assert!(
        deny.seen.lock().unwrap().is_empty(),
        "no phase re-gated on resume"
    );

    // The lease is released when a run returns, so the next run over the same
    // directory is not blocked by its predecessor.
    assert!(
        crate::holder(&dir).is_none(),
        "no lease left behind after the run"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_second_run_is_refused_while_another_holds_the_artifact_dir() {
    // Spec 19's single-writer problem, at the runner level: the GUI and the CLI
    // resolve the SAME `specs/<slug>/` for the same task, and before the lease
    // both would write it — the second silently clobbering whatever decision the
    // first had recorded.
    //
    // The foreign lease here stands in for a live GUI run parked at its gate. The
    // refusal must name it, or a user has nothing to act on.
    let backend = full_backend();
    let ws = temp("lease-contention");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // A live lease held by another process (a pid that is not ours).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(
        dir.join("lease.json"),
        serde_json::to_string(&crate::Lease {
            owner_pid: std::process::id().wrapping_add(1),
            owner: "sc-win".into(),
            acquired_ms: now,
            heartbeat_ms: now,
            run_token: 1,
        })
        .unwrap(),
    )
    .unwrap();

    let err = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .expect_err("must refuse rather than race the holder");

    let msg = err.to_string();
    assert!(msg.contains("sc-win"), "names the holder: {msg}");
    assert!(msg.contains("held by"), "{msg}");
    // Refused before doing anything: no artifacts, and the holder's lease intact.
    assert!(
        !dir.join("spec.md").exists(),
        "a refused run writes nothing"
    );
    assert_eq!(
        crate::holder(&dir).unwrap().owner,
        "sc-win",
        "the holder's lease is untouched"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_resumed_run_shows_the_pending_draft_rather_than_regenerating_it() {
    // Spec 19's "come back to a drafted spec" case. Overnight the phone showed a
    // drafted spec; the machine rebooted before anyone approved it. Restoring only
    // the APPROVED history would discard that draft and regenerate a *different*
    // artifact — so the developer approves something they never reviewed.
    let ws = temp("resume-draft");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // A parked run: a draft saved, its gate never answered.
    let mut parked = crate::state::WorkflowState::new(task);
    parked.set(Artifact::draft(
        Phase::Specs,
        "# THE DRAFT THE PHONE SHOWED",
    ));
    crate::state::save_to(&dir, &mut parked, true).unwrap();

    // Resume. The backend would happily generate something different if asked.
    let backend = full_backend();
    let seen: Mutex<Vec<(Phase, String)>> = Mutex::new(Vec::new());
    let outcome = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|p, c| seen.lock().unwrap().push((p, c.to_string())),
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();

    // The human is shown the artifact they were already looking at.
    let specs_shown: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| *p == Phase::Specs)
        .map(|(_, c)| c.clone())
        .collect();
    assert!(
        specs_shown
            .iter()
            .any(|c| c.contains("THE DRAFT THE PHONE SHOWED")),
        "the pending draft was restored, not regenerated: {specs_shown:?}"
    );
    // And it is the restored draft that got gated and approved — not a fresh one.
    assert_eq!(
        outcome.state.artifact(Phase::Specs).unwrap().content,
        "# THE DRAFT THE PHONE SHOWED"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_resumed_run_keeps_the_frozen_contract_tests() {
    // Spec 19: "A daemon restart silently unfreezing the contract tests is a
    // correctness bug, not a papercut." Held only in memory, `test_files` started
    // empty on resume — so nothing downstream knew which tests were frozen, and a
    // worker could rewrite them to pass.
    let ws = temp("resume-frozen");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // A prior run approved its design and froze two contract tests.
    let mut prior = crate::state::WorkflowState::new(task);
    for p in [
        Phase::Specs,
        Phase::Architecture,
        Phase::Layout,
        Phase::StageBreakdown,
    ] {
        prior.set(Artifact::draft(p, format!("# {}", p.title())));
        prior.approve(p);
    }
    prior.set_test_files(vec!["test_a.py".into(), "test_b.py".into()]);
    crate::state::save_to(&dir, &mut prior, true).unwrap();

    let backend = full_backend();
    let outcome = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();

    assert_eq!(
        outcome.test_files,
        vec!["test_a.py".to_string(), "test_b.py".to_string()],
        "the approved contract survived the resume"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn two_different_tasks_that_slugify_alike_do_not_share_a_directory() {
    // Spec 19's "Task identity": the slug is the first sentence capped at 40
    // chars, so "Fix the bug. In auth" and "Fix the bug. In the parser" produce
    // the same directory. For phone-typed free text, short generic first
    // sentences are the NORMAL case — and the second run would silently adopt the
    // first's approved artifacts as its own and then overwrite them.
    let ws = temp("slug-collision");
    let first = "Fix the bug. In auth";
    let second = "Fix the bug. In the parser";

    let (dir_a, _) = crate::artifact_dirs(first, &ws);
    let (dir_b, _) = crate::artifact_dirs(second, &ws);
    assert_eq!(dir_a, dir_b, "these really do collide");
    let dir = dir_a.unwrap();

    let backend = full_backend();
    run_workflow_moded_to(
        &backend,
        &backend,
        first,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();

    let err = run_workflow_moded_to(
        &backend,
        &backend,
        second,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .expect_err("a different task must not adopt this one's design");

    let msg = err.to_string();
    assert!(msg.contains("different task"), "{msg}");
    assert!(
        msg.contains("In auth"),
        "names what is already there: {msg}"
    );

    // The first task's work is untouched.
    let on_disk = crate::state::load_from(&dir).unwrap().unwrap();
    assert!(
        on_disk.task.starts_with("Fix the bug. In auth"),
        "{}",
        on_disk.task
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_same_task_resuming_is_not_a_collision() {
    // The Breakdown→Build handoff runs the SAME task twice against one directory.
    // A collision check that fired here would break the feature it protects.
    let ws = temp("same-task-resume");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    let backend = full_backend();

    for _ in 0..2 {
        run_workflow_moded_to(
            &backend,
            &backend,
            task,
            &ws,
            ThinkPolicy::default(),
            WorkflowMode::full_tdd(),
            &|_, _| {},
            &AutoApprove,
            Some(&dir),
            true,
            &mut |_, _| {},
        )
        .expect("the same task resumes cleanly");
    }
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_corrupt_state_json_is_an_error_not_a_silent_fresh_start() {
    // Spec 19: a corrupt or truncated state.json was "swallowed and silently
    // restarted from the top" — discarding an entire approved design and quietly
    // re-running work a human had signed off, with nothing to point at.
    let ws = temp("corrupt-state");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("state.json"), "{ truncated ").unwrap();

    let backend = full_backend();
    let err = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .expect_err("a corrupt state must not be silently discarded");

    let msg = err.to_string();
    assert!(msg.contains("could not be read"), "{msg}");
    assert!(
        msg.contains("move it aside"),
        "says how to proceed deliberately: {msg}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn task_conflict_compares_the_whole_first_line_not_the_first_sentence() {
    use super::drive::task_conflict;

    // THE case spec 19 names. Both slugify to `fix-the-bug`, because the slug is
    // cut at the first sentence — so agreeing there and differing after is
    // precisely the collision, not an exemption. An earlier cut of this check
    // compared sentences and therefore never fired.
    assert_eq!(
        task_conflict("Fix the bug. In auth", "Fix the bug. In the parser"),
        Some("Fix the bug. In auth")
    );

    // The same task resuming is not a conflict, however much grounding was
    // appended to the stored copy — grounding appends, so the original is the
    // prefix.
    assert_eq!(task_conflict("Add seat types", "Add seat types"), None);
    assert_eq!(
        task_conflict("Add seat types\n\n=== survey ===\nfoo.rs", "Add seat types"),
        None
    );
    assert_eq!(
        task_conflict("Add seat types", "Add seat types for crew roles"),
        None,
        "a refined task is a continuation, not a different one"
    );

    // Nothing to compare against yields no conflict rather than a false alarm.
    assert_eq!(task_conflict("", "anything"), None);
    assert_eq!(task_conflict("anything", ""), None);
}

#[test]
fn a_restored_draft_is_actually_gated_and_the_run_completes() {
    // The bug the first cut of the draft-restore shipped with, and which its own
    // test missed. `next_phase()` returns the first phase with NO artifact, so a
    // restored draft — which has one — was skipped entirely: never gated, left
    // `Draft` forever, and `is_complete()` false, which is what the CLI and GUI
    // use to decide a run finished.
    //
    // The earlier test asserted only that the draft's CONTENT survived, which was
    // true precisely *because* the phase was skipped. Assert the gate ran.
    let ws = temp("draft-gated");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    let mut parked = crate::state::WorkflowState::new(task);
    parked.set(Artifact::draft(
        Phase::Specs,
        "# THE DRAFT THE PHONE SHOWED",
    ));
    crate::state::save_to(&dir, &mut parked, true).unwrap();

    let backend = full_backend();
    let gate = ScriptedGate::new(vec![
        Decision::Approve,
        Decision::Approve,
        Decision::Approve,
        Decision::Approve,
        Decision::Approve,
    ]);
    let outcome = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &gate,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();

    // The restored draft was put to the human, not silently skipped.
    assert!(
        gate.seen.lock().unwrap().contains(&Phase::Specs),
        "the restored draft must be gated: {:?}",
        gate.seen.lock().unwrap()
    );
    // And having been approved, it is approved — not left Draft forever.
    assert!(
        outcome.state.artifact(Phase::Specs).unwrap().is_approved(),
        "a gated-and-approved draft ends approved"
    );
    assert_eq!(
        outcome.state.artifact(Phase::Specs).unwrap().content,
        "# THE DRAFT THE PHONE SHOWED",
        "and it is still the artifact the human reviewed"
    );
    assert!(
        outcome.state.is_complete(),
        "the run completes; a skipped phase would poison is_complete() forever"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn send_back_notes_survive_a_resume() {
    // A phase sent back with notes must regenerate WITH them, even across a
    // restart. The notes were persisted all along; the resuming run just never
    // copied them onto its freshly-grounded state, so the regeneration happened
    // as if nobody had said anything.
    let ws = temp("resume-feedback");
    let task = "Add seat types for crew roles";
    let (artifact_dir, _) = crate::artifact_dirs(task, &ws);
    let dir = artifact_dir.unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // A parked run whose Architecture was sent back with a note.
    let mut parked = crate::state::WorkflowState::new(task);
    parked.set(Artifact::draft(Phase::Specs, "# spec"));
    parked.approve(Phase::Specs);
    parked.set_feedback(Phase::Architecture, "make it event-driven");
    crate::state::save_to(&dir, &mut parked, true).unwrap();

    // The note has to reach the prompt of the phase that regenerates.
    let backend = full_backend();
    let outcome = run_workflow_moded_to(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::full_tdd(),
        &|_, _| {},
        &AutoApprove,
        Some(&dir),
        true,
        &mut |_, _| {},
    )
    .unwrap();

    // Approving Architecture clears its note, so the run ending clean is the
    // proof it was carried and consumed rather than dropped on load.
    assert!(outcome.state.is_complete());
    assert_eq!(
        outcome.state.feedback(Phase::Architecture),
        None,
        "the note was consumed by the regeneration it asked for"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
