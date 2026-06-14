//! M7 exit-criterion test (spec 07/08): a task that decomposes into multiple
//! subtasks is completed by workers and integrated green — including a dependency
//! (one subtask runs only after another), proving the DAG-ordered waves work, and
//! that the swarm emits its orchestration events.

use std::sync::Mutex;

use dc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use dc_proto::Result;
use dc_swarm::{run_swarm, FnSwarmSink, SwarmConfig, SwarmEvent};

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
        (
            "set VALUE to 1",
            vec![
                r#"{"tool":"edit_file","path":"mod_a.py","old_str":"VALUE = 0","new_str":"VALUE = 1"}"#,
                r#"{"tool":"finish"}"#,
            ],
        ),
        (
            "set RESULT to 2",
            vec![
                r#"{"tool":"edit_file","path":"mod_b.py","old_str":"RESULT = 0","new_str":"RESULT = 2"}"#,
                r#"{"tool":"finish"}"#,
            ],
        ),
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
