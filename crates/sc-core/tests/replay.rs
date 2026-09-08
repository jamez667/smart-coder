//! Model-free regression test for the agent loop (spec 03 — "Determinism & replay").
//!
//! Replay a recorded session's model replies through the loop and assert it makes the
//! same tool calls, in the same order, and stops for the same reason as the recording.
//! If a harness change alters what the loop does with identical model output, this
//! fails — with no model, no network, and no VRAM involved.
//!
//! The fixture is self-describing: the expected calls are read from the SAME NDJSON
//! the replies come from (`recorded_calls`), so a re-recorded run needs no hand-edited
//! expectation list.
//!
//! TODO: replace `scripted-3-turn.ndjson` with a recorded engine-grid-scan run from
//! `sc-eval --agent --only engine-grid-scan --log <dir>`. The current fixture was
//! produced by the `record_scripted_fixture` test below driving a `MockBackend`, so it
//! exercises the plumbing, not a real model's behaviour.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sc_core::{
    run_agent_observed, AgentConfig, AgentEvent, FnSink, JsonLinesSink, ParseRepair, StopReason,
};
use sc_model::{MockBackend, ReplayBackend};
use sc_tools::default_registry;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("replay")
}

/// The task the fixture was recorded against. Part of the fixture contract: a
/// different task changes the prompt, and a real model would reply differently.
const TASK: &str = "make answer() return 42 in lib.rs";

/// A fresh workspace holding a copy of the fixture's source files (the run edits it).
fn temp_workspace(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-replay-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let p = entry.unwrap().path();
        // Only the workspace files, not the recording itself.
        if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            std::fs::copy(&p, d.join(p.file_name().unwrap())).unwrap();
        }
    }
    d
}

/// The config the fixture was recorded with — identical on record and replay, or the
/// loop legitimately diverges.
fn config() -> AgentConfig {
    AgentConfig {
        verify_command: None,
        ..AgentConfig::default()
    }
}

/// The `(tool, arg)` of every `ToolCall` event in an NDJSON session log, in order.
fn recorded_calls(ndjson: &str) -> Vec<(String, String)> {
    ndjson
        .lines()
        .filter_map(|l| serde_json::from_str::<AgentEvent>(l).ok())
        .filter_map(|e| match e {
            AgentEvent::ToolCall { tool, arg } => Some((tool, arg)),
            _ => None,
        })
        .collect()
}

/// The `Stopped { reason }` of an NDJSON session log (the last one, should a log hold
/// several runs).
fn recorded_stop(ndjson: &str) -> Option<StopReason> {
    ndjson
        .lines()
        .filter_map(|l| serde_json::from_str::<AgentEvent>(l).ok())
        .filter_map(|e| match e {
            AgentEvent::Stopped { reason } => Some(reason),
            _ => None,
        })
        .next_back()
}

#[test]
fn a_replayed_run_makes_the_recorded_tool_calls_in_order() {
    let fixture = fixtures_dir().join("scripted-3-turn.ndjson");
    let ndjson = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture.display()));
    let expected_calls = recorded_calls(&ndjson);
    let expected_stop = recorded_stop(&ndjson).expect("the recording ends with Stopped");
    assert!(
        !expected_calls.is_empty(),
        "the fixture must hold at least one ToolCall"
    );

    let ws = temp_workspace("replay");
    let backend = ReplayBackend::from_ndjson(&fixture).unwrap();
    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &ParseRepair,
        TASK,
        &ws,
        &config(),
        &sink,
    )
    .unwrap();
    let events = log.into_inner().unwrap();

    let actual_calls: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { tool, arg } => Some((tool.clone(), arg.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        actual_calls, expected_calls,
        "the loop made different tool calls from the recording"
    );

    let actual_stop = match events.last() {
        Some(AgentEvent::Stopped { reason }) => reason.clone(),
        other => panic!("the run must end with Stopped, got {other:?}"),
    };
    assert_eq!(
        actual_stop, expected_stop,
        "the loop stopped for a different reason"
    );

    // Every recorded reply was used and no more were asked for — the turn count is
    // part of the behaviour under test.
    assert_eq!(
        backend.remaining(),
        0,
        "the loop did not consume every recorded turn"
    );
    assert_eq!(
        backend.prompts().len(),
        actual_calls.len(),
        "one prompt per recorded tool call"
    );

    // The edit landed exactly as recorded, so the workspace is the recorded end state.
    let edited = std::fs::read_to_string(ws.join("lib.rs")).unwrap();
    assert!(
        edited.contains("42"),
        "the recorded edit must apply: {edited}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// Re-record the fixture from a scripted mock. Run with
/// `cargo test -p sc-core --test replay record_scripted_fixture -- --ignored`
/// after a deliberate harness change that legitimately alters the event stream.
#[test]
#[ignore]
fn record_scripted_fixture() {
    let ws = temp_workspace("record");
    let backend = MockBackend::new([
        r#"{"tool":"read_file","path":"lib.rs"}"#,
        r#"{"tool":"edit_file","path":"lib.rs","old_str":"    41","new_str":"    42"}"#,
        // No `summary`: the default registry's `finish` takes none (only the
        // read-only registry's does), and an unknown parameter is a repair turn.
        r#"{"tool":"finish"}"#,
    ]);
    let out = fixtures_dir().join("scripted-3-turn.ndjson");
    let file = std::fs::File::create(&out).unwrap();
    let sink = JsonLinesSink::new(file);
    run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &ParseRepair,
        TASK,
        &ws,
        &config(),
        &sink,
    )
    .unwrap();
    drop(sink.into_inner());

    let ndjson = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        recorded_calls(&ndjson).len(),
        3,
        "three tool calls recorded"
    );
    assert_eq!(recorded_stop(&ndjson), Some(StopReason::Finished));
    let _ = std::fs::remove_dir_all(&ws);
}
