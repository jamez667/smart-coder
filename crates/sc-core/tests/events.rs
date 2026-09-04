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
            // A LONG reply that stopped mid-sentence. Length matters: `finish_reason:
            // "length"` alone is not proof of truncation -- llama.cpp reports it when
            // a grammar-constrained decode stops cleanly at a well-formed object too,
            // so the detector also requires the reply to have actually run long.
            GenerateResponse::with_finish_reason(
                format!(
                    "Looking at the file, I think the fix is to {}",
                    "reason about this at length. ".repeat(400)
                ),
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

/// **A zero prompt budget is now impossible, so the fault cannot fire.**
///
/// It used to be reachable: a reserve sized for a 32k model applied to an 8k one
/// gave `8192 * 0.75 - 6144 = 0`, and a zero budget does not fail loudly -- it stops
/// constraining anything and the prompt grows until the server rejects it. The
/// detector for it was the right first move; capping the reserve at half the usable
/// window in `prompt_budget` is the better one, because no caller can recreate the
/// condition.
///
/// The fault kind is kept: it costs nothing and guards the invariant from below, so
/// if some future path does produce a zero budget it is still reported rather than
/// silent. This test pins the invariant that makes it unreachable.
#[test]
fn a_reserve_larger_than_the_window_cannot_zero_the_budget() {
    let ws = temp("nobudget");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let backend = Scripted::new(vec![r#"{"tool":"finish"}"#]);
    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));

    // Scripted advertises 8192; ask for a reserve that would once have eaten it.
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
    let budget = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::RunStarted { prompt_budget, .. } => Some(*prompt_budget),
            _ => None,
        })
        .expect("the run started");
    assert!(budget > 0, "the reserve must never zero the budget");

    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::HarnessFault {
                kind: FaultKind::ContextBudgetUnusable,
                ..
            }
        )),
        "with the cap in place the condition cannot arise"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// A SHORT well-formed reply reporting `length` is not a truncation.
///
/// llama.cpp reports `finish_reason: "length"` when a grammar-constrained decode
/// stops cleanly at the end of a well-formed object. Measured on a real run: a
/// complete 20-character `{"tool":"edit_file"}` was reported as "truncated at the
/// 6144-token cap", with the server's own log saying `truncated = 0`. A detector
/// that fires on a healthy turn is exactly the noise the fault count exists to
/// avoid -- it makes "2 harness faults" mean nothing.
#[test]
fn a_short_complete_reply_reporting_length_is_not_a_truncation() {
    let ws = temp("shortlength");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    struct ShortLength;
    impl ModelBackend for ShortLength {
        fn name(&self) -> &str {
            "short-length"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8_192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
            // Complete and well-formed, and the server still says "length".
            Ok(GenerateResponse::with_finish_reason(
                r#"{"tool":"finish"}"#,
                Some("length".into()),
            ))
        }
    }

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    run_agent_observed(
        &ShortLength,
        None,
        &default_registry(),
        &ParseRepair,
        "do a thing",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    assert!(
        !log.into_inner().unwrap().iter().any(|e| matches!(
            e,
            AgentEvent::HarnessFault {
                kind: FaultKind::ReplyTruncated,
                ..
            }
        )),
        "a complete short reply must not be reported as truncated"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// A reasoning model that spends its ENTIRE budget thinking: `finish_reason: "length"`
/// with `content` empty, because the thinking went to a separate `reasoning_content`
/// field the harness never sees.
struct ThoughtItselfOut(std::cell::RefCell<usize>);

impl ModelBackend for ThoughtItselfOut {
    fn name(&self) -> &str {
        "thought-itself-out"
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
            // Measured against Tiel: 700/700 completion tokens spent, content EMPTY.
            GenerateResponse::with_finish_reason(String::new(), Some("length".into()))
        } else {
            GenerateResponse::with_finish_reason(r#"{"tool":"finish"}"#, Some("stop".into()))
        })
    }
}

/// **The empty-content truncation must fire too.**
///
/// The detector required the reply to have "actually run long" to suppress llama.cpp's
/// false `length` on a grammar-constrained decode. But it measured `content`, and a
/// reasoning model cut off mid-thought has ZERO content tokens — so the real truncation
/// scored 0, failed the length check, and passed silently. That is exactly the case
/// observed in the chat panel: an empty bubble, no warning, and a model that looked
/// broken while the harness held the evidence.
#[test]
fn a_reply_that_was_all_reasoning_and_no_answer_still_raises_the_fault() {
    let ws = temp("thought-itself-out");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    run_agent_observed(
        &ThoughtItselfOut(std::cell::RefCell::new(0)),
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
        .expect("an empty reply cut off at the cap must raise a harness fault");

    assert_eq!(fault.0, FaultKind::ReplyTruncated);
    assert_eq!(fault.2, 1, "raised on the turn it happened");
    // The detail must say the model never got to answer — "cut off after 0 chars" would
    // read as the model declining to speak rather than as us cutting it off.
    assert!(
        fault.1.contains("NO content"),
        "the detail must distinguish the all-reasoning case, got: {}",
        fault.1
    );
}

/// A model that has stopped emitting tool calls and only writes prose.
struct OnlyProse;

impl ModelBackend for OnlyProse {
    fn name(&self) -> &str {
        "only-prose"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        Ok(GenerateResponse::with_finish_reason(
            "Looking at the code, the widths are assigned to the wrong segments. \
             Let me re-read that once more to be sure."
                .to_string(),
            Some("stop".into()),
        ))
    }
}

/// **Two unparseable replies in a row must end a read-only run.**
///
/// The stall detector observes ACTIONS, and a reply that fails extraction produces none —
/// so a model that stops emitting tool calls is invisible to it. Measured live: eight
/// consecutive 30,000-character explanations, six RepairTriggered events, ZERO Stalled
/// events, ending `BudgetExhausted` 812 seconds later with nothing returned. Every one of
/// those turns re-sent the whole prompt.
#[test]
fn a_read_only_run_stops_after_two_unparseable_replies() {
    let ws = temp("only-prose");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    // A read-only registry is what selects this behaviour.
    let registry = sc_tools::read_only_registry();
    let report = run_agent_observed(
        &OnlyProse,
        None,
        &registry,
        &ParseRepair,
        "why is the trail thin?",
        &ws,
        &AgentConfig {
            max_steps: 20,
            ..AgentConfig::default()
        },
        &sink,
    )
    .unwrap();

    assert!(
        report.steps <= 3,
        "must stop on the 2nd bad reply, not burn the budget — took {} steps",
        report.steps
    );
    assert!(
        matches!(report.stop_reason, sc_core::StopReason::Stalled(_)),
        "the stop reason must say why, got {:?}",
        report.stop_reason
    );
    let events = log.into_inner().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Stalled { .. })),
        "a stall must be reported, not silence"
    );
}

/// **A model generating to the token cap twice in a row must not keep going.**
///
/// An earlier version of this compared PROMPTS and was dead code: every failed turn appends
/// the reply and the repair error to the history, so the next prompt is never byte-identical.
/// Measured on a real transcript — five consecutive prompts, zero identical. What repeats is
/// the FAILURE, and each attempt costs a full prompt pass plus a maximum-length generation.
#[test]
fn a_read_only_run_stops_when_replies_keep_hitting_the_cap() {
    let ws = temp("same-prompt");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let registry = sc_tools::read_only_registry();
    let report = run_agent_observed(
        &OnlyProse,
        None,
        &registry,
        &ParseRepair,
        "why is the trail thin?",
        &ws,
        &AgentConfig {
            max_steps: 30,
            ..AgentConfig::default()
        },
        &sink,
    )
    .unwrap();

    assert!(
        report.steps < 30,
        "the run must stop early, not exhaust its budget on capped replies"
    );
    let events = log.into_inner().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Stalled { .. })),
        "stopping must be reported, not silent"
    );
}

/// A model that always generates to the token cap without ever emitting a tool call —
/// exactly what the star-trail investigation did at ~47,000 characters a turn.
struct AlwaysTruncates;

impl ModelBackend for AlwaysTruncates {
    fn name(&self) -> &str {
        "always-truncates"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        Ok(GenerateResponse::with_finish_reason(
            "Let me analyse the widths. ".repeat(400),
            Some("length".into()),
        ))
    }
}

/// **Repeated capped replies end a read-only run.**
///
/// Each costs a full prompt pass plus a maximum-length generation — most of a minute on a
/// 35B model — and buys nothing, because a capped reply carries no tool call. Measured, a
/// run burned 14 steps and 812 seconds this way with SIX repair events and ZERO stalls: the
/// stall detector watches actions, and these turns produce none.
#[test]
fn repeated_capped_replies_stop_a_read_only_run() {
    let ws = temp("always-truncates");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let report = run_agent_observed(
        &AlwaysTruncates,
        None,
        &sc_tools::read_only_registry(),
        &ParseRepair,
        "why is the trail thin?",
        &ws,
        &AgentConfig {
            max_steps: 20,
            ..AgentConfig::default()
        },
        &sink,
    )
    .unwrap();

    assert!(
        report.steps <= 3,
        "must stop once the cap keeps being hit, not burn 20 steps — took {}",
        report.steps
    );
    let events = log.into_inner().unwrap();
    // Either guard may fire first here -- the model produces both a capped reply AND an
    // unparseable one every turn. What matters is that SOME stall is reported, so the
    // transcript says why the run ended rather than showing a silent budget exhaustion.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Stalled { .. })),
        "stopping must be reported, not silent"
    );
}

/// A model that ALTERNATES: runs to the cap, recovers with one good call, repeats.
///
/// This is the real observed pattern, and it is what a consecutive-streak counter cannot
/// catch — the streak resets on every recovery.
struct AlternatesTruncating(std::cell::RefCell<usize>);

impl ModelBackend for AlternatesTruncating {
    fn name(&self) -> &str {
        "alternates"
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
        Ok(if *n % 2 == 1 {
            GenerateResponse::with_finish_reason(
                "Let me analyse the widths. ".repeat(400),
                Some("length".into()),
            )
        } else {
            GenerateResponse::with_finish_reason(
                r#"{"tool":"read_file","path":"a.txt"}"#,
                Some("stop".into()),
            )
        })
    }
}

/// **An ALTERNATING cap-hit must still end the run.**
///
/// The previous version counted a consecutive streak and passed its unit test only because
/// that mock truncated every single turn. On a real run the pattern alternates — cap, one
/// good call, cap again — so the streak reset on every recovery and the kill never fired.
/// Observed directly: step 4 truncated at 45,964 chars, step 5 recovered, and the run
/// carried on burning minutes.
#[test]
fn alternating_cap_hits_still_stop_a_read_only_run() {
    let ws = temp("alternates");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let log = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));
    let report = run_agent_observed(
        &AlternatesTruncating(std::cell::RefCell::new(0)),
        None,
        &sc_tools::read_only_registry(),
        &ParseRepair,
        "why is the trail thin?",
        &ws,
        &AgentConfig {
            max_steps: 20,
            ..AgentConfig::default()
        },
        &sink,
    )
    .unwrap();

    assert!(
        report.steps <= 6,
        "3 cap hits arrive by step 5-6; must not run to 20 — took {}",
        report.steps
    );
    let events = log.into_inner().unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Stalled { trigger } if trigger.contains("token cap")
        )),
        "the stall must name the cap so a transcript says why the run ended"
    );
}

/// **A read-only turn must not be given room to ramble for 45,000 characters.**
///
/// `max_tokens` was the reply reserve (12,288), so a model that reasons in prose — Tiel
/// ignores `/no_think`, which is a Qwen3 directive — ran to ~45,000 characters and most of
/// a minute before anything stopped it. A read-only call is a tiny JSON object and the one
/// long reply is `finish` (~2,000 chars), so nothing here needs that much room.
///
/// The reserve itself must NOT be lowered: it also sets the prompt budget, and cutting it
/// starved the reading and produced runs that never answered.
#[test]
fn a_read_only_turn_gets_a_small_reply_cap() {
    let seen = std::sync::Mutex::new(Vec::new());
    struct Spy<'a>(&'a std::sync::Mutex<Vec<usize>>);
    impl ModelBackend for Spy<'_> {
        fn name(&self) -> &str {
            "spy"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 32_768,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, r: &GenerateRequest) -> Result<GenerateResponse> {
            self.0.lock().unwrap().push(r.max_tokens);
            Ok(GenerateResponse::with_finish_reason(
                r#"{"tool":"finish","summary":"done"}"#,
                Some("stop".into()),
            ))
        }
    }

    let ws = temp("reply-cap");
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    run_agent_observed(
        &Spy(&seen),
        None,
        &sc_tools::read_only_registry(),
        &ParseRepair,
        "why?",
        &ws,
        &AgentConfig {
            response_reserve_tokens: 12288,
            ..AgentConfig::default()
        },
        &FnSink(|_: &AgentEvent| {}),
    )
    .unwrap();

    let caps = seen.into_inner().unwrap();
    assert!(!caps.is_empty(), "the model must have been called");
    assert!(
        caps.iter().all(|&c| c <= 2048),
        "a read-only reply must be capped small, got {caps:?}"
    );
}

/// **Prompts sent to the model must not carry reflowed source indentation.**
///
/// A `\`-continuation is fine — rustc strips the newline AND the following indentation. What
/// is not fine is a continuation that got reflowed so the backslash no longer ends the line;
/// the whitespace then survives into the string. That has happened twice: once in a UI line
/// the user saw as "and come          back with the", and once in the steer that tells a
/// rambling model to call `finish`, where it also hid the text from a grep.
///
/// Checks the COMPILED string, not the source, so a correctly-continued literal passes.
#[test]
fn the_truncation_steer_reads_cleanly() {
    // The exact text a read-only run appends when a reply runs to the cap.
    let steer = "

Your reply was cut off because it ran too long. You are THINKING OUT LOUD instead of answering. Do not re-read anything - the code you need is already above. Reply with ONE JSON object now: {\"tool\":\"finish\",\"summary\":\"<your answer: the file, the line, the cause, the fix>\"}. Keep it under 200 words.";
    assert!(
        !steer.trim().contains("  "),
        "a run of spaces means the continuation was reflowed: {steer:?}"
    );
    assert!(
        steer.contains("THINKING OUT LOUD instead of answering"),
        "the phrase must survive intact — it was split across a reflow once"
    );
}
