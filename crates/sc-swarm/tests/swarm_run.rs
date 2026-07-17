//! M7 exit-criterion test (spec 07/08): a task that decomposes into multiple
//! subtasks is completed by workers and integrated green — including a dependency
//! (one subtask runs only after another), proving the DAG-ordered waves work, and
//! that the swarm emits its orchestration events.

use std::sync::Mutex;

use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use sc_swarm::{run_swarm, FnSwarmSink, NullSwarmSink, SwarmConfig, SwarmEvent};

/// A scripted, thread-safe backend: routes replies by a substring of the prompt.
struct Scripted {
    scripts: Mutex<Vec<(String, Vec<String>)>>,
}
impl Scripted {
    fn new(scripts: Vec<(&str, Vec<&str>)>) -> Self {
        Self {
            scripts: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
                    .collect(),
            ),
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
    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        let instr = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut scripts = self.scripts.lock().unwrap();
        for (key, queue) in scripts.iter_mut() {
            if instr.contains(key.as_str()) && !queue.is_empty() {
                return Ok(GenerateResponse {
                    content: queue.remove(0),
                });
            }
        }
        Ok(GenerateResponse {
            content: r#"{"tool":"finish"}"#.to_string(),
        })
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-swarm-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn decomposes_runs_in_dependency_order_and_integrates_all() {
    let ws = temp("dag");
    std::fs::write(ws.join("mod_a.py").as_path(), "VALUE = 0\n").unwrap();
    std::fs::write(ws.join("mod_b.py").as_path(), "import mod_a\nRESULT = 0\n").unwrap();

    // Orchestrator: subtask `a` (edit mod_a), then `b` depends on `a` (edit mod_b).
    let backend = Scripted::new(vec![
        (
            "Break the coding task",
            vec![
                r#"[
                {"id":"a","goal":"set VALUE to 1 in mod_a.py","files":["mod_a.py"],"deps":[]},
                {"id":"b","goal":"set RESULT to 2 in mod_b.py","files":["mod_b.py"],"deps":["a"]}
            ]"#,
            ],
        ),
        // Merge calls (orchestrator) — keyed on "File: <path>" — return the full
        // corrected file. Proposer calls (worker) — keyed on the goal — return the
        // worker's text proposal (also the corrected file here).
        ("File: mod_a.py", vec!["VALUE = 1\n"]),
        ("File: mod_b.py", vec!["import mod_a\nRESULT = 2\n"]),
        ("set VALUE to 1", vec!["VALUE = 1\n"]),
        ("set RESULT to 2", vec!["import mod_a\nRESULT = 2\n"]),
    ]);

    let events = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| events.lock().unwrap().push(e.clone()));

    let report = run_swarm(
        &backend,
        &backend,
        None,
        "update both modules",
        "",
        &ws,
        &SwarmConfig::default(),
        &sink,
    );

    // Both subtasks integrated.
    assert!(report.all_done, "all subtasks should complete: {report:?}");
    assert_eq!(report.done, 2);
    assert_eq!(
        std::fs::read_to_string(ws.join("mod_a.py")).unwrap(),
        "VALUE = 1\n"
    );
    assert!(std::fs::read_to_string(ws.join("mod_b.py"))
        .unwrap()
        .contains("RESULT = 2"));

    let ev = events.into_inner().unwrap();
    // The decomposition prompt is emitted first, then the Decomposed board; SwarmDone
    // last.
    assert!(matches!(
        ev.first(),
        Some(SwarmEvent::OrchestratorPrompt { .. })
    ));
    assert!(ev
        .iter()
        .any(|e| matches!(e, SwarmEvent::Decomposed { .. })));
    assert!(matches!(
        ev.last(),
        Some(SwarmEvent::SwarmDone { all_done: true, .. })
    ));
    // `a` was started and integrated before `b` started (dependency order).
    let pos = |pred: &dyn Fn(&SwarmEvent) -> bool| ev.iter().position(pred).unwrap();
    let a_integrated = pos(
        &|e| matches!(e, SwarmEvent::Integrated { subtask, accepted: true, .. } if subtask == "a"),
    );
    let b_started =
        pos(&|e| matches!(e, SwarmEvent::WorkerStarted { subtask, .. } if subtask == "b"));
    assert!(a_integrated < b_started, "b must start after a integrates");

    let _ = std::fs::remove_dir_all(&ws);
}

/// The retry loop (spec 08 — "Subtask retry on partial or rejected integration").
/// A worker lands a **partial** fix on attempt 1 (`X = 1`) that the cumulative gate
/// accepts (failing count flat) but that leaves the subtask's scoped test red. The
/// orchestrator must NOT mark the subtask done — it must re-dispatch the subtask
/// with failing-test feedback, and on attempt 2 the worker produces the correct fix
/// (`X = 2`), the scoped test goes green, and the run reports `all_done`. Mirrors
/// `partial_fix_that_leaves_the_suite_red_is_not_reported_done` but proves recovery
/// rather than just honest failure.
#[test]
fn an_incomplete_subtask_is_retried_with_feedback_until_its_tests_pass() {
    let ws = temp("retry-recover");
    std::fs::write(ws.join("target.py").as_path(), "X = 0\n").unwrap();
    std::fs::write(
        ws.join("test_target.py").as_path(),
        "from target import X\n\n\ndef test_x_is_two():\n    assert X == 2\n",
    )
    .unwrap();

    // Attempt 1 yields `X = 1` (partial: still red, but failing count flat so the
    // cumulative gate accepts the bytes). Attempt 2 — re-dispatched with feedback —
    // yields `X = 2` (green). The Scripted backend pops a key's queue in order, so
    // the same goal/merge keys serve attempt 1 then attempt 2.
    let backend = Scripted::new(vec![
        (
            "Break the coding task",
            vec![
                r#"[{"id":"t","goal":"set X to 2 in target.py","files":["target.py"],"deps":[]}]"#,
            ],
        ),
        ("File: target.py", vec!["X = 1\n", "X = 2\n"]),
        ("set X to 2", vec!["X = 1\n", "X = 2\n"]),
    ]);

    let events = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| events.lock().unwrap().push(e.clone()));

    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
        frozen_paths: vec!["test_target.py".to_string()],
        ..SwarmConfig::default()
    };
    let report = run_swarm(&backend, &backend, None, "fix target", "", &ws, &cfg, &sink);

    // The subtask recovered: it's genuinely done and the suite is green.
    assert_eq!(
        report.done, 1,
        "subtask should be done after retry: {report:?}"
    );
    assert_eq!(report.failed, 0, "no failures after recovery: {report:?}");
    assert!(
        report.all_done,
        "the run is green after the retry: {report:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("target.py")).unwrap(),
        "X = 2\n"
    );

    // Exactly one SubtaskRetry was emitted (attempt 1 of 2), carrying the still-red
    // test name as feedback.
    let ev = events.into_inner().unwrap();
    let retries: Vec<&SwarmEvent> = ev
        .iter()
        .filter(|e| matches!(e, SwarmEvent::SubtaskRetry { .. }))
        .collect();
    assert_eq!(retries.len(), 1, "one retry expected: {ev:?}");
    match retries[0] {
        SwarmEvent::SubtaskRetry {
            subtask,
            attempt,
            max,
            failing_tests,
        } => {
            assert_eq!(subtask, "t");
            assert_eq!(*attempt, 1);
            assert_eq!(*max, 2);
            assert!(
                failing_tests.iter().any(|t| t.contains("test_x_is_two")),
                "feedback names the failing test: {failing_tests:?}"
            );
        }
        _ => unreachable!(),
    }

    let _ = std::fs::remove_dir_all(&ws);
}

/// With `max_subtask_retries = 0`, the retry loop is disabled (today's behaviour):
/// a partial fix integrates, the scoped check is skipped, and the run stops honestly
/// on the final whole-suite verify (no retry attempted). Guards the `0` escape hatch.
#[test]
fn zero_retries_restores_no_retry_behaviour() {
    let ws = temp("retry-zero");
    std::fs::write(ws.join("target.py").as_path(), "X = 0\n").unwrap();
    std::fs::write(
        ws.join("test_target.py").as_path(),
        "from target import X\n\n\ndef test_x_is_two():\n    assert X == 2\n",
    )
    .unwrap();

    let backend = Scripted::new(vec![
        (
            "Break the coding task",
            vec![
                r#"[{"id":"t","goal":"set X to 2 in target.py","files":["target.py"],"deps":[]}]"#,
            ],
        ),
        ("File: target.py", vec!["X = 1\n", "X = 2\n"]),
        ("set X to 2", vec!["X = 1\n", "X = 2\n"]),
    ]);

    let events = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| events.lock().unwrap().push(e.clone()));

    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
        frozen_paths: vec!["test_target.py".to_string()],
        max_subtask_retries: 0,
        ..SwarmConfig::default()
    };
    let report = run_swarm(&backend, &backend, None, "fix target", "", &ws, &cfg, &sink);

    // No retry attempted; the partial fix landed and the final verify says not-done.
    assert!(
        !report.all_done,
        "0 retries → honest stop, not done: {report:?}"
    );
    let ev = events.into_inner().unwrap();
    assert!(
        !ev.iter()
            .any(|e| matches!(e, SwarmEvent::SubtaskRetry { .. })),
        "no retry events when max_subtask_retries == 0"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// Advisor escalation before the FINAL retry (spec 08 — "the orchestrator *may*
/// escalate to the advisor for a one-line nudge before the final attempt"). With
/// max_subtask_retries=1, attempt 1 lands a partial fix; before the single (final)
/// retry the orchestrator consults the advisor, folds its one-line hint into the
/// worker's prompt, and the final attempt — now nudged — passes. Proves: the
/// AdvisorConsulted event fires exactly once, the advice reaches the worker, and the
/// subtask recovers.
#[test]
fn advisor_is_consulted_before_the_final_retry() {
    let ws = temp("advisor");
    std::fs::write(ws.join("target.py").as_path(), "X = 0\n").unwrap();
    std::fs::write(
        ws.join("test_target.py").as_path(),
        "from target import X\n\n\ndef test_x_is_two():\n    assert X == 2\n",
    )
    .unwrap();

    // Worker/orchestrator: attempt 1 → X=1 (partial), attempt 2 → X=2 (green). The
    // proposer/merge queues pop in order across the two attempts.
    let backend = Scripted::new(vec![
        (
            "Break the coding task",
            vec![
                r#"[{"id":"t","goal":"set X to 2 in target.py","files":["target.py"],"deps":[]}]"#,
            ],
        ),
        ("File: target.py", vec!["X = 1\n", "X = 2\n"]),
        ("set X to 2", vec!["X = 1\n", "X = 2\n"]),
    ]);
    // A SEPARATE advisor backend, keyed on its own system-prompt phrase so it never
    // collides with the worker/orchestrator routing. It returns a terse hint.
    let advisor = Scripted::new(vec![(
        "senior engineer",
        vec!["The literal must be 2, not 1 — set X = 2."],
    )]);

    let events = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| events.lock().unwrap().push(e.clone()));

    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
        frozen_paths: vec!["test_target.py".to_string()],
        max_subtask_retries: 1, // a single retry → that retry IS the final one
        ..SwarmConfig::default()
    };
    let report = run_swarm(
        &backend,
        &backend,
        Some(&advisor),
        "fix target",
        "",
        &ws,
        &cfg,
        &sink,
    );

    assert!(report.all_done, "nudged final retry recovers: {report:?}");
    assert_eq!(report.done, 1);
    assert_eq!(
        std::fs::read_to_string(ws.join("target.py")).unwrap(),
        "X = 2\n"
    );

    // The advisor was consulted exactly once, before the final retry, carrying the hint.
    let ev = events.into_inner().unwrap();
    let consults: Vec<&SwarmEvent> = ev
        .iter()
        .filter(|e| matches!(e, SwarmEvent::AdvisorConsulted { .. }))
        .collect();
    assert_eq!(consults.len(), 1, "one advisor consult expected: {ev:?}");
    match consults[0] {
        SwarmEvent::AdvisorConsulted { subtask, advice } => {
            assert_eq!(subtask, "t");
            assert!(advice.contains("set X = 2"), "advice carried: {advice}");
        }
        _ => unreachable!(),
    }
    // The consult happened AFTER the retry was announced and BEFORE integration.
    let retry_pos = ev
        .iter()
        .position(|e| matches!(e, SwarmEvent::SubtaskRetry { .. }))
        .unwrap();
    let consult_pos = ev
        .iter()
        .position(|e| matches!(e, SwarmEvent::AdvisorConsulted { .. }))
        .unwrap();
    assert!(
        retry_pos < consult_pos,
        "consult follows the retry announce"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// No advisor configured → no consult, but the retry loop still runs (degrades to a
/// clean "no senior to ask", mirroring sc_core::advisor). Guards that escalation is
/// strictly optional.
#[test]
fn no_advisor_means_no_consult_but_retry_still_runs() {
    let ws = temp("no-advisor");
    std::fs::write(ws.join("target.py").as_path(), "X = 0\n").unwrap();
    std::fs::write(
        ws.join("test_target.py").as_path(),
        "from target import X\n\n\ndef test_x_is_two():\n    assert X == 2\n",
    )
    .unwrap();
    let backend = Scripted::new(vec![
        (
            "Break the coding task",
            vec![
                r#"[{"id":"t","goal":"set X to 2 in target.py","files":["target.py"],"deps":[]}]"#,
            ],
        ),
        ("File: target.py", vec!["X = 1\n", "X = 2\n"]),
        ("set X to 2", vec!["X = 1\n", "X = 2\n"]),
    ]);
    let events = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| events.lock().unwrap().push(e.clone()));
    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
        frozen_paths: vec!["test_target.py".to_string()],
        max_subtask_retries: 1,
        ..SwarmConfig::default()
    };
    let report = run_swarm(&backend, &backend, None, "fix target", "", &ws, &cfg, &sink);

    assert!(report.all_done, "recovers without an advisor: {report:?}");
    let ev = events.into_inner().unwrap();
    assert!(
        !ev.iter()
            .any(|e| matches!(e, SwarmEvent::AdvisorConsulted { .. })),
        "no consult without an advisor"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Regression for a real bug found live (2026-06-14): a worker can land a
/// **partial** fix that the per-merge "didn't make it worse" gate accepts (failing
/// count unchanged), so every subtask integrates and the board is all-done — yet
/// the suite is still red. Spec 08 step 5 requires a **final integration
/// verification** before `finish`; without it the swarm reported `all_done: true`
/// over a red suite, violating the honest-stop-line (spec 06). The run must report
/// `all_done == false` when the final suite isn't green.
#[test]
fn partial_fix_that_leaves_the_suite_red_is_not_reported_done() {
    let ws = temp("partial");
    // The target file and a frozen pytest that demands X == 2.
    std::fs::write(ws.join("target.py").as_path(), "X = 0\n").unwrap();
    std::fs::write(
        ws.join("test_target.py").as_path(),
        "from target import X\n\n\ndef test_x_is_two():\n    assert X == 2\n",
    )
    .unwrap();

    // The worker/orchestrator only ever get X to 1 — closer, but still failing.
    // Before: 1 failing. After merging `X = 1`: still 1 failing → the cumulative
    // gate (after <= before) ACCEPTS it. The board then reads all-done.
    let backend = Scripted::new(vec![
        (
            "Break the coding task",
            vec![
                r#"[{"id":"t","goal":"set X to 2 in target.py","files":["target.py"],"deps":[]}]"#,
            ],
        ),
        ("File: target.py", vec!["X = 1\n"]),
        ("set X to 2", vec!["X = 1\n"]),
    ]);

    // `max_subtask_retries: 0` isolates the final-integration-verify backstop this
    // test was written to prove (no retry loop recovering the partial fix). With the
    // gate accepting the no-worse merge, the subtask integrates `Done`, yet the final
    // whole-suite verify must still report the run not-done.
    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
        max_subtask_retries: 0,
        ..SwarmConfig::default()
    };
    let report = run_swarm(
        &backend,
        &backend,
        None,
        "fix target",
        "",
        &ws,
        &cfg,
        &NullSwarmSink,
    );

    // The subtask integrated (the per-merge gate accepted the no-worse change)...
    assert_eq!(report.done, 1, "the partial fix integrates: {report:?}");
    // ...but the suite is still red, so the run must NOT claim done.
    assert!(
        !report.all_done,
        "a red final suite must not be reported as done: {report:?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
