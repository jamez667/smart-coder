//! M7 exit-criterion test (spec 07/08): a task that decomposes into multiple
//! subtasks is completed by workers and integrated green — including a dependency
//! (one subtask runs only after another), proving the DAG-ordered waves work, and
//! that the swarm emits its orchestration events.

use std::sync::Mutex;

use dc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use dc_proto::Result;
use dc_swarm::{run_swarm, FnSwarmSink, NullSwarmSink, SwarmConfig, SwarmEvent};

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
        "dc-swarm-it-{tag}-{}-{}",
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
            "orchestrator for a swarm",
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
    // Decomposed first, SwarmDone last.
    assert!(matches!(ev.first(), Some(SwarmEvent::Decomposed { .. })));
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
            "orchestrator for a swarm",
            vec![
                r#"[{"id":"t","goal":"set X to 2 in target.py","files":["target.py"],"deps":[]}]"#,
            ],
        ),
        ("File: target.py", vec!["X = 1\n"]),
        ("set X to 2", vec!["X = 1\n"]),
    ]);

    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
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
