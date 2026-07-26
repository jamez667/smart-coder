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

    // All six phases ran, in order, and the workflow is complete.
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
