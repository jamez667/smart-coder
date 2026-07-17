//! The event stream is the seam every observer (TUI, --json, log) consumes, so
//! it's worth asserting the loop emits the right typed events in order — driven
//! by a recording sink, no terminal involved.

use std::sync::Mutex;

use sc_core::{run_agent_observed, AgentConfig, AgentEvent, FnSink, ParseRepair, StopReason};
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use sc_tools::default_registry;

struct Scripted(std::cell::RefCell<Vec<String>>);
impl Scripted {
    fn new(t: Vec<&str>) -> Self {
        Scripted(std::cell::RefCell::new(
            t.into_iter().map(String::from).collect(),
        ))
    }
}
impl ModelBackend for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        let mut s = self.0.borrow_mut();
        let content = if s.len() > 1 {
            s.remove(0)
        } else {
            s.first()
                .cloned()
                .unwrap_or_else(|| r#"{"tool":"finish"}"#.into())
        };
        Ok(GenerateResponse { content })
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-events-{tag}-{}-{}",
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
fn emits_the_expected_event_sequence() {
    let ws = temp("seq");
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let backend = Scripted::new(vec![
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"finish"}"#,
    ]);

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    run_agent_observed(
        &backend,
        None,
        &registry,
        &ParseRepair,
        "read a.txt",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    let events = log.into_inner().unwrap();

    // Starts with RunStarted, ends with Stopped(Finished).
    assert!(matches!(
        events.first(),
        Some(AgentEvent::RunStarted { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Stopped {
            reason: StopReason::Finished
        })
    ));
    // A ModelTurn precedes the read tool call.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ModelTurn { .. })));
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::ToolCall { tool, arg } if tool == "read_file" && arg == "a.txt")));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolResult { .. })));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn emits_stall_and_stop_when_looping_without_advisor() {
    let ws = temp("stall");
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let backend = Scripted::new(vec![r#"{"tool":"read_file","path":"a.txt"}"#]);
    let cfg = AgentConfig {
        max_steps: 20,
        repeat_limit: 3,
        ..Default::default()
    };

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    run_agent_observed(
        &backend,
        None,
        &registry,
        &ParseRepair,
        "loop",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let events = log.into_inner().unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Stalled { .. })));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Stopped {
            reason: StopReason::Stalled(_)
        })
    ));
    let _ = std::fs::remove_dir_all(&ws);
}
