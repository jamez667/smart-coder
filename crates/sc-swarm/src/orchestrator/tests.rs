use super::*;
use crate::event::NullSwarmSink;
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use std::sync::Mutex as StdMutex;

#[test]
fn own_tests_maps_source_files_to_their_tests_by_stem() {
    let frozen = vec![
        "test_app.py".to_string(),
        "index.test.js".to_string(),
        "style.test.js".to_string(),
    ];
    // app.py → test_app.py only (not the frontend tests).
    assert_eq!(
        own_tests(&["app.py".to_string()], &frozen),
        vec!["test_app.py"]
    );
    // index.html → index.test.js (frontend test by stem).
    assert_eq!(
        own_tests(&["templates/index.html".to_string()], &frozen),
        vec!["index.test.js"]
    );
    // A source file with no matching test → empty (caller falls back to the suite).
    assert!(own_tests(&["nope.py".to_string()], &frozen).is_empty());
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-swarm-orch-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A backend that maps a per-subtask script: it inspects the instruction to
/// decide what to emit. Thread-safe (Sync) so workers can share it.
struct ScriptedSwarm {
    // instruction-substring -> queued replies
    scripts: StdMutex<Vec<(String, Vec<String>)>>,
}
impl ScriptedSwarm {
    fn new(scripts: Vec<(&str, Vec<&str>)>) -> Self {
        Self {
            scripts: StdMutex::new(
                scripts
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
                    .collect(),
            ),
        }
    }
}
impl ModelBackend for ScriptedSwarm {
    fn name(&self) -> &str {
        "scripted-swarm"
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
                return Ok(GenerateResponse::new(queue.remove(0)));
            }
        }
        Ok(GenerateResponse::new(r#"{"tool":"finish"}"#.to_string()))
    }
}

#[test]
fn two_independent_subtasks_propose_and_merge() {
    let ws = temp("two");
    std::fs::write(ws.join("a.txt"), "old-a").unwrap();
    std::fs::write(ws.join("b.txt"), "old-b").unwrap();

    // Flow per subtask: orchestrator decomposes -> tiny worker PROPOSES the
    // corrected file as text -> orchestrator MERGES the proposal into the file.
    // The merge prompt contains "--- CURRENT ---" (the proposer's doesn't), so we
    // key the merge replies on that and the proposer replies on the goal.
    let backend = ScriptedSwarm::new(vec![
        // decomposition
        (
            "Break the coding task",
            vec![
                r#"[{"id":"a","goal":"set a.txt to new-a","files":["a.txt"]},{"id":"b","goal":"set b.txt to new-b","files":["b.txt"]}]"#,
            ],
        ),
        // Merge calls (orchestrator) — prompt contains "File: <path>"; key on that.
        ("File: a.txt", vec!["new-a"]),
        ("File: b.txt", vec!["new-b"]),
        // Proposer calls (worker) — prompt contains the goal; key on that.
        ("set a.txt to new-a", vec!["new-a"]),
        ("set b.txt to new-b", vec!["new-b"]),
    ]);

    let report = run_swarm(
        &backend,
        &backend,
        None,
        "update a and b",
        "",
        &ws,
        &SwarmConfig::default(),
        &NullSwarmSink,
    );

    assert!(
        report.all_done,
        "both subtasks should integrate: {report:?}"
    );
    assert_eq!(report.done, 2);
    // The merge normalizes to a single trailing newline.
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "new-a\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("b.txt")).unwrap(),
        "new-b\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
#[ignore = "live: drives a real `python -m pytest` verify; needs python on PATH"]
fn a_merge_that_breaks_the_suite_is_rejected_and_reverted() {
    let ws = temp("reject");
    // A working impl + a frozen pytest that passes for it. Python (not `sh`) so
    // the verify command is portable across platforms (incl. Windows CI).
    std::fs::write(
        ws.join("calc.py"),
        "def is_even(n):\n    return n % 2 == 0\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("test_calc.py"),
        "from calc import is_even\n\n\ndef test_even():\n    assert is_even(4)\n",
    )
    .unwrap();

    // The worker proposes a broken impl; the orchestrator merges it; the suite
    // goes red, so the merge is reverted and the subtask fails.
    let broken = "def is_even(n):\n    return False\n";
    let backend = ScriptedSwarm::new(vec![
        (
            "Break the coding task",
            vec![r#"[{"id":"x","goal":"break calc.py badly","files":["calc.py"]}]"#],
        ),
        // merge (keyed on "File: <path>") and proposer (keyed on goal) both yield
        // the broken version.
        ("File: calc.py", vec![broken]),
        ("break calc.py badly", vec![broken]),
    ]);

    let cfg = SwarmConfig {
        verify_command: Some("python -m pytest -q".to_string()),
        ..Default::default()
    };
    let report = run_swarm(
        &backend,
        &backend,
        None,
        "break it",
        "",
        &ws,
        &cfg,
        &NullSwarmSink,
    );

    assert!(!report.all_done);
    assert_eq!(report.failed, 1);
    // calc.py was reverted to the working version (integration rejected it).
    let impl_after = std::fs::read_to_string(ws.join("calc.py")).unwrap();
    assert!(
        impl_after.contains("n % 2 == 0"),
        "should be reverted: {impl_after}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_merge_never_overwrites_a_frozen_contract_test() {
    let ws = temp("frozen");
    std::fs::write(ws.join("test_it.py"), "FROZEN CONTRACT\n").unwrap();
    std::fs::write(ws.join("impl.py"), "old\n").unwrap();

    // One subtask whose worker proposes to rewrite BOTH impl.py and the frozen
    // test. The merge applies impl.py but must leave the test untouched.
    let backend = ScriptedSwarm::new(vec![
        (
            "Break the coding task",
            vec![r#"[{"id":"x","goal":"do it","files":["impl.py","test_it.py"]}]"#],
        ),
        ("do it", vec!["new impl"]),
        ("File: impl.py", vec!["new\n"]),
        ("File: test_it.py", vec!["HACKED\n"]),
    ]);

    let cfg = SwarmConfig {
        frozen_paths: vec!["test_it.py".to_string()],
        ..Default::default()
    };
    let _ = run_swarm_board(
        &backend,
        &backend,
        None,
        crate::board::TaskBoard::new(vec![crate::board::Subtask::new("x", "do it")
            .with_files(vec!["impl.py".into(), "test_it.py".into()])]),
        &ws,
        &cfg,
        &NullSwarmSink,
    );

    // The frozen test is byte-for-byte intact; impl.py got the merge.
    assert_eq!(
        std::fs::read_to_string(ws.join("test_it.py")).unwrap(),
        "FROZEN CONTRACT\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
