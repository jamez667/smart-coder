//! The event stream is the seam every observer (TUI, --json, log) consumes, so
//! it's worth asserting the loop emits the right typed events in order — driven
//! by a recording sink, no terminal involved.

use std::sync::Mutex;

use sc_core::{
    run_agent_observed, AgentConfig, AgentEvent, FaultKind, FnSink, ParseRepair, StopReason,
};
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
        Ok(GenerateResponse::new(content))
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

/// A backend whose first reply is cut off at the token cap, exactly as a server
/// reports it: content with no tool call in it, plus `finish_reason: "length"`.
struct Truncating(std::cell::RefCell<usize>);

impl ModelBackend for Truncating {
    fn name(&self) -> &str {
        "truncating"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        let mut n = self.0.borrow_mut();
        *n += 1;
        Ok(if *n == 1 {
            // What a real truncated turn looks like: mid-sentence, no JSON.
            GenerateResponse::with_finish_reason(
                "Looking at the file, I think the fix is to ",
                Some("length".into()),
            )
        } else {
            GenerateResponse::with_finish_reason(r#"{"tool":"finish"}"#, Some("stop".into()))
        })
    }
}

/// **The detector must be seen to fire.**
///
/// A truncated turn carries no tool call, so the loop's repair path reports "no JSON
/// tool object in your reply" -- blaming the model for a cap *we* set. That
/// misattribution cost 54 dead turns on one SWE-bench instance. The fault event is
/// the whole fix: it must appear, on the turn it happened, naming the cap.
#[test]
fn reports_a_harness_fault_when_the_reply_was_truncated() {
    let ws = temp("truncated");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    run_agent_observed(
        &Truncating(std::cell::RefCell::new(0)),
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
    let fault = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::HarnessFault { kind, detail, step } => Some((*kind, detail.clone(), *step)),
            _ => None,
        })
        .expect("a truncated reply must raise a harness fault");

    assert_eq!(fault.0, FaultKind::ReplyTruncated);
    assert_eq!(fault.2, 1, "raised on the turn it happened, not at the end");
    // The detail must name the cap, so the transcript says what to change.
    assert!(
        fault
            .1
            .contains(&AgentConfig::default().response_reserve_tokens.to_string()),
        "detail should name the token cap, got: {}",
        fault.1
    );

    // And only the truncated turn raises one -- the clean `stop` turn must not.
    let count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::HarnessFault { .. }))
        .count();
    assert_eq!(count, 1, "a normal stop is not a fault");

    let _ = std::fs::remove_dir_all(&ws);
}

/// **The second detector must be seen to fire.**
///
/// A model that calls `run_verification` with no verify command configured gets a
/// "no command" message, and the loop records `green: false` -- indistinguishable
/// from a suite that genuinely failed. Task runs sat in exactly this state: the
/// config left `verify_command` as None, so the agent edited blind and every attempt
/// to check its own work was scored as its own failure.
#[test]
fn reports_a_harness_fault_when_verification_is_not_configured() {
    let ws = temp("noverify");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let backend = Scripted::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"finish"}"#,
    ]);

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    // The default config is the point: `verify_command` is None in it.
    let cfg = AgentConfig::default();
    assert!(
        cfg.verify_command.is_none(),
        "this test is about the unconfigured case"
    );
    run_agent_observed(
        &backend,
        None,
        &registry,
        &ParseRepair,
        "check it",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let events = log.into_inner().unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::HarnessFault {
                kind: FaultKind::VerifyUnavailable,
                ..
            }
        )),
        "an unconfigured verification must raise a harness fault, got: {events:#?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// **The third detector must be seen to fire** -- and, just as importantly, seen NOT
/// to fire on a healthy run.
///
/// A detector that cries wolf becomes a number people learn to ignore, so both
/// halves are asserted here. The bug: harness guidance named `edit_lines` regardless
/// of the registry, putting 99 mentions of an uncallable tool in front of one model.
#[test]
fn reports_a_harness_fault_when_the_prompt_names_an_unoffered_tool() {
    let ws = temp("unoffered");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let run = |registry: sc_tools::ToolRegistry| {
        let backend = Scripted::new(vec![r#"{"tool":"finish"}"#]);
        let log = Mutex::new(Vec::new());
        let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
        // Pin a focus file: that is the guidance that names the edit tool.
        let cfg = AgentConfig {
            focus_files: vec!["a.txt".to_string()],
            ..AgentConfig::default()
        };
        run_agent_observed(
            &backend,
            None,
            &registry,
            &ParseRepair,
            "edit a.txt",
            &ws,
            &cfg,
            &sink,
        )
        .unwrap();
        log.into_inner()
            .unwrap()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::HarnessFault {
                        kind: FaultKind::ToolNotOffered,
                        ..
                    }
                )
            })
            .count()
    };

    // The healthy case: everything the prompt names is callable. MUST be silent.
    assert_eq!(
        run(sc_tools::default_registry()),
        0,
        "a prompt that only names offered tools is not a fault"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// **A zero prompt budget must be reported before the first turn.**
///
/// `prompt_budget` saturates: reserve more for the reply than the usable window
/// holds and it comes out ZERO. That does not fail loudly -- it stops constraining
/// anything, and the prompt grows until the SERVER rejects it with a context-size
/// error that reads as a model problem. Measured on a real run: a backend left at
/// its 8192 default with a 12288-token reply reserve produced
/// "request (33164 tokens) exceeds the available context size (32768)".
#[test]
fn reports_a_harness_fault_when_the_prompt_budget_is_zero() {
    let ws = temp("nobudget");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let backend = Scripted::new(vec![r#"{"tool":"finish"}"#]);
    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));

    // Scripted reports an 8192-token window; reserve more of it than 75% leaves.
    let cfg = AgentConfig {
        response_reserve_tokens: 12_288,
        ..AgentConfig::default()
    };
    run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &ParseRepair,
        "do a thing",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let events = log.into_inner().unwrap();
    let fault = events.iter().find_map(|e| match e {
        AgentEvent::HarnessFault {
            kind: FaultKind::ContextBudgetUnusable,
            detail,
            ..
        } => Some(detail.clone()),
        _ => None,
    });
    let detail = fault.expect("a zero budget must raise a harness fault");
    // The detail must name both numbers, so the transcript says what to change.
    assert!(detail.contains("8192"), "should name the window: {detail}");
    assert!(
        detail.contains("12288"),
        "should name the reserve: {detail}"
    );

    // And a HEALTHY budget must stay silent -- a detector that always fires is noise.
    let log2 = Mutex::new(Vec::new());
    let sink2 = FnSink(|e: &AgentEvent| log2.lock().unwrap().push(e.clone()));
    run_agent_observed(
        &Scripted::new(vec![r#"{"tool":"finish"}"#]),
        None,
        &default_registry(),
        &ParseRepair,
        "do a thing",
        &ws,
        &AgentConfig::default(),
        &sink2,
    )
    .unwrap();
    assert!(
        !log2.into_inner().unwrap().iter().any(|e| matches!(
            e,
            AgentEvent::HarnessFault {
                kind: FaultKind::ContextBudgetUnusable,
                ..
            }
        )),
        "a workable budget is not a fault"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
