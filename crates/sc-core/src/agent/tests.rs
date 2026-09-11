//! Integration tests for the agent loop (`run_agent_observed` and friends).
//!
//! These drive the whole act→observe→recover cycle end to end; the narrower unit tests
//! for the extracted helpers live beside those helpers (see `dispatch`, `prompt`, `window`).

use super::*;
use sc_model::{CallbackBackend, Capabilities, GenerateResponse, MockBackend, ToolCalling};
use serde_json::json;

use super::escalation::{DIAGNOSIS_LIMIT, SELF_RECOVERY_LIMIT};
use super::test_util::temp_dir;

/// A `CallbackBackend` whose closure returns a fixed reply — a scriptable "model"
/// for loop/stall tests. Native tool-calling, generous context.
fn scripted_backend<F>(name: &'static str, generate: F) -> CallbackBackend<F>
where
    F: Fn(&sc_model::GenerateRequest) -> sc_proto::Result<GenerateResponse>,
{
    let caps = Capabilities {
        max_context_tokens: 128_000,
        tool_calling: ToolCalling::OpenAiStyle,
        on_device: false,
    };
    CallbackBackend::new(name, caps, generate)
}

/// **A pinned seed and temperature reach the backend, on EVERY turn.**
///
/// The loop built its request with `GenerateRequest::new` and never touched sampling, so
/// there was no route from config to the sampler at all: every measured run drew at 0.2
/// with a server-chosen seed. `rust-two-stage` run ten times on one commit came back
/// 1 pass / 9 red, diverging on a single turn -- the ladder could not tell a fix from a
/// lucky draw.
///
/// Asserted per turn, not just on the first: a seed applied once and dropped afterwards
/// would look right in a one-turn test and still leave the run unreproducible.
#[test]
fn a_pinned_seed_and_temperature_reach_every_request() {
    use std::sync::Mutex;

    let ws = temp_dir("seeded");
    let seen: Mutex<Vec<(Option<u64>, f32)>> = Mutex::new(Vec::new());
    let replies = Mutex::new(vec![
        json!({"tool":"write_file","path":"a.txt","content":"hi"}).to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);
    let backend = scripted_backend("seeded", |req: &sc_model::GenerateRequest| {
        seen.lock().unwrap().push((req.seed, req.temperature));
        let mut r = replies.lock().unwrap();
        Ok(GenerateResponse::new(if r.is_empty() {
            json!({"tool":"finish"}).to_string()
        } else {
            r.remove(0)
        }))
    });

    let cfg = AgentConfig {
        seed: Some(4242),
        temperature: Some(0.0),
        ..AgentConfig::default()
    };
    let report = run_agent(&backend, "write a.txt", &ws, &cfg).unwrap();
    assert!(report.steps >= 2, "the run must have taken turns");

    let seen = seen.into_inner().unwrap();
    assert!(!seen.is_empty(), "the backend was called");
    for (i, (seed, temp)) in seen.iter().enumerate() {
        assert_eq!(*seed, Some(4242), "turn {i} lost the seed");
        assert_eq!(*temp, 0.0, "turn {i} lost the temperature");
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// The other half of the contract: an UNPINNED run must keep the backend's own sampling.
/// Forcing a default seed would make every interactive session replay one draw.
#[test]
fn an_unpinned_run_leaves_sampling_to_the_backend() {
    use std::sync::Mutex;

    let ws = temp_dir("unseeded");
    let seen: Mutex<Vec<(Option<u64>, f32)>> = Mutex::new(Vec::new());
    let backend = scripted_backend("unseeded", |req: &sc_model::GenerateRequest| {
        seen.lock().unwrap().push((req.seed, req.temperature));
        Ok(GenerateResponse::new(json!({"tool":"finish"}).to_string()))
    });

    let _ = run_agent(&backend, "do nothing", &ws, &AgentConfig::default()).unwrap();

    let seen = seen.into_inner().unwrap();
    assert!(!seen.is_empty(), "the backend was called");
    for (seed, temp) in &seen {
        assert_eq!(*seed, None, "an unpinned run must not invent a seed");
        assert_eq!(*temp, 0.2, "and must keep GenerateRequest's own default");
    }
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn writes_a_file_then_finishes() {
    let ws = temp_dir("write");
    let backend = MockBackend::new([
        json!({"tool":"write_file","path":"out.txt","content":"hi"}).to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);

    let report = run_agent(&backend, "create out.txt", &ws, &AgentConfig::default()).unwrap();
    assert!(report.finished);
    assert_eq!(report.steps, 2);
    assert_eq!(report.metrics.valid, 2);
    assert_eq!(report.metrics.invalid, 0);
    assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "hi");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
#[ignore = "live: drives a real `python -m pytest` verify; needs python on PATH"]
fn diagnosis_fires_on_a_test_stall_then_is_bounded() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("diagnose");
    // Seed DIFFERENT from what the model writes, so its first write is a real change (which
    // triggers the auto-verify), and every identical write after is a no-op (the stall).
    std::fs::write(ws.join("a.txt"), "seed").unwrap();

    // The worker LOOPS on a no-op read forever — UNLESS the request is the diagnostic
    // pass (its system prompt says "ROOT-CAUSE analysis"), in which case it returns a
    // diagnosis. So the loop stalls, the diagnosis fires, and (because the model keeps
    // looping after) it stalls again — letting us assert the bound.
    let caps = sc_model::Capabilities {
        max_context_tokens: 8192,
        tool_calling: sc_model::ToolCalling::None,
        on_device: false,
    };
    // The model writes the SAME file content every turn: the first write changes the
    // workspace (triggering the auto-verify, which records a RED verification), and
    // subsequent identical writes don't change it → a no-progress stall, where the
    // diagnosis fires off the STORED red output. On a diagnostic request it returns a
    // diagnosis.
    let backend = CallbackBackend::new("loop-or-diagnose", caps, |req: &GenerateRequest| {
        let is_diag = req
            .messages
            .iter()
            .any(|m| m.content.contains("ROOT-CAUSE analysis"));
        let content = if is_diag {
            "FILE: a.txt\nLINE: 1\nCAUSE: the value is wrong\nFIX: set it right".to_string()
        } else {
            r#"{"tool":"write_file","path":"a.txt","content":"x"}"#.to_string()
        };
        Ok(GenerateResponse::new(content))
    });

    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let cfg = AgentConfig {
        max_steps: 40,
        repeat_limit: 3,
        no_progress_limit: 3,
        // A host verify command that prints a parseable RED result and exits non-zero, so
        // the auto-verify records a failing Verification the diagnosis can read.
        verify_command: Some(
            "python -c \"print('test_app.py::test_x FAILED'); import sys; sys.exit(1)\""
                .to_string(),
        ),
        diagnose: true,
        ..AgentConfig::default()
    };
    run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let evs = events.lock().unwrap();
    let diagnoses = evs
        .iter()
        .filter(|e| matches!(e, AgentEvent::Diagnosis { .. }))
        .count();
    // It fired (the model debugs blind, the harness diagnoses) and is bounded.
    assert!(diagnoses >= 1, "a diagnosis should fire on a test stall");
    assert!(
        diagnoses <= DIAGNOSIS_LIMIT,
        "diagnoses must be bounded to {DIAGNOSIS_LIMIT}, got {diagnoses}"
    );
    // The diagnosis report reached the model as an observation.
    assert!(
        evs.iter().any(|e| matches!(
            e,
            AgentEvent::Diagnosis { report, .. } if report.contains("CAUSE:")
        )),
        "the diagnosis carries a root-cause report"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_referenced_plan_is_pinned_so_the_model_never_re_reads_it() {
    // Reproduces the live bug: an iterate run told to "implement PLAN-lakes.md" saw the model
    // read the plan over and over (the plan wasn't pinned, so each read scrolled out of the
    // window and it fetched it again). The fix pins the plan's contents every turn and
    // short-circuits a redundant read. This test drives a model that ALWAYS tries to read the
    // plan first, and asserts the harness (a) shows the plan body in the prompt and (b) answers
    // the read with "ALREADY SHOWN" instead of the file — so the loop can't spin on it.
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("plan-pin");
    std::fs::write(
        ws.join("PLAN-lakes.md"),
        "## Plan: lakes\n**Approach:** flood-fill basins below water level.\n\
         **Files to touch:**\n- water.rs (new)\n**Steps:**\n1. detect basins\n2. emit quads",
    )
    .unwrap();

    // The model's script: turn 1 tries to read the plan (the reflex that used to loop); once it
    // has been told the plan is already shown, it writes the file and finishes. It keys off the
    // observation it got back, which rides in the recent window of the next prompt.
    let caps = sc_model::Capabilities {
        max_context_tokens: 8192,
        tool_calling: sc_model::ToolCalling::None,
        on_device: false,
    };
    let backend = CallbackBackend::new("plan-reader", caps, |req: &GenerateRequest| {
        // The model reflexively reads the plan until the harness tells it the plan is shown;
        // then it writes the planned file, and once the write's success observation comes back
        // it finishes. Keying off the observations (not raw history) avoids finishing before
        // the write actually lands.
        let prompt: String = req.messages.iter().map(|m| m.content.clone()).collect();
        let told_shown = prompt.contains("ALREADY SHOWN");
        let write_confirmed = prompt.contains("write_file water.rs ok");
        let content = if !told_shown {
            r#"{"tool":"read_file","path":"PLAN-lakes.md"}"#.to_string()
        } else if !write_confirmed {
            r#"{"tool":"write_file","path":"water.rs","content":"// lakes"}"#.to_string()
        } else {
            r#"{"tool":"finish"}"#.to_string()
        };
        Ok(GenerateResponse::new(content))
    });

    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let cfg = AgentConfig {
        max_steps: 12,
        repeat_limit: 3,
        no_progress_limit: 3,
        ..AgentConfig::default()
    };
    // Verbose so PromptAssembled carries the full prompt — lets us assert the plan body is pinned.
    let cfg = AgentConfig {
        verbose: true,
        ..cfg
    };
    let report = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "Implement the feature plan in PLAN-lakes.md. Follow its Steps.",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let evs = events.lock().unwrap();

    // (a) The plan BODY is pinned into the assembled prompt — not just its filename.
    let plan_pinned = evs.iter().any(|e| {
        matches!(e, AgentEvent::PromptAssembled { messages, .. }
            if messages.iter().any(|m| m.content.contains("flood-fill basins")))
    });
    assert!(
        plan_pinned,
        "the plan body must be pinned into the prompt every turn"
    );

    // (b) A read of the pinned plan is short-circuited to the ALREADY-SHOWN note, so the model
    //     can't spin on it — the observation the harness fed back says so.
    let short_circuited = evs.iter().any(|e| {
        matches!(e, AgentEvent::ToolResult { full, .. } if full.contains("ALREADY SHOWN")
            && full.contains("PLAN-lakes.md"))
    });
    assert!(
        short_circuited,
        "a read of the pinned plan must be short-circuited, not re-run"
    );

    // (c) The run made real progress and finished (it wrote the file once unblocked), rather
    //     than looping on the plan read until the step budget ran out.
    assert!(
        report.finished,
        "the run should finish once the plan-read loop is broken"
    );
    assert!(
        ws.join("water.rs").is_file(),
        "the model wrote the planned file"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn no_diagnosis_when_the_flag_is_off_or_no_verify_command() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let run = |diagnose: bool, verify: Option<&str>| -> usize {
        let ws = temp_dir("no-diag");
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        // A backend that loops forever on a no-op read (so the run stalls), via a
        // callback (MockBackend errors once exhausted).
        let caps = sc_model::Capabilities {
            max_context_tokens: 8192,
            tool_calling: sc_model::ToolCalling::None,
            on_device: false,
        };
        let backend = CallbackBackend::new("looper", caps, |_req: &GenerateRequest| {
            Ok(GenerateResponse::new(
                r#"{"tool":"read_file","path":"a.txt"}"#.to_string(),
            ))
        });
        let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
        let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
        let registry = sc_tools::default_registry();
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let cfg = AgentConfig {
            max_steps: 20,
            repeat_limit: 3,
            verify_command: verify.map(String::from),
            diagnose,
            ..AgentConfig::default()
        };
        run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            "x",
            &ws,
            &cfg,
            &sink,
        )
        .unwrap();
        let n = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, AgentEvent::Diagnosis { .. }))
            .count();
        let _ = std::fs::remove_dir_all(&ws);
        n
    };
    // Flag off ⇒ never; flag on but no verify command ⇒ never (not a test-driven run).
    assert_eq!(run(false, Some("echo x")), 0, "flag off → no diagnosis");
    assert_eq!(run(true, None), 0, "no verify command → no diagnosis");
}

#[test]
fn reading_an_already_pinned_file_is_short_circuited() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("pinned-read");
    std::fs::write(ws.join("app.py"), "PINNED_CONTENT_MARKER = 1\n").unwrap();
    std::fs::write(ws.join("other.py"), "OTHER_CONTENT_MARKER = 2\n").unwrap();

    // Turn 1: read the focused (pinned) file → must be redirected, NOT executed.
    // Turn 2: read an UNPINNED file → must run normally (returns its content).
    // Turn 3: finish.
    let backend = MockBackend::new([
        json!({"tool":"read_file","path":"app.py"}).to_string(),
        json!({"tool":"read_file","path":"other.py"}).to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);
    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let cfg = AgentConfig {
        focus_files: vec!["app.py".to_string()],
        ..AgentConfig::default()
    };
    run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "edit app.py",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let evs = events.lock().unwrap();
    let results: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult { full, .. } => Some(full.as_str()),
            _ => None,
        })
        .collect();
    // The pinned read was redirected (no file content; a "already shown" note).
    assert!(
        results.iter().any(|r| r.contains("ALREADY SHOWN IN FULL")),
        "reading the pinned focus file must be short-circuited: {results:?}"
    );
    // The pinned file's body did NOT come back via a read (it's only in the prompt).
    assert!(
        !results.iter().any(|r| r.contains("PINNED_CONTENT_MARKER")),
        "the pinned read must not return file content"
    );
    // The UNPINNED read ran for real and returned its content.
    assert!(
        results.iter().any(|r| r.contains("OTHER_CONTENT_MARKER")),
        "an unpinned read must still execute: {results:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn focused_run_pins_its_file_plus_imported_bodies_and_maps_the_rest() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("focus-map");
    // Focused app.py imports store (→ store.py's FULL body shown) but NOT util (→ util.py
    // only as a signature, body absent). Tests the import-aware split.
    std::fs::write(
        ws.join("app.py"),
        "from store import add\n\ndef handler():\n    return add(1)\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("store.py"),
        "IMPORTED_BODY_MARKER = 42\n\ndef add(n):\n    return n + 1\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("util.py"),
        "UNIMPORTED_BODY_MARKER = 99\n\ndef helper():\n    return 0\n",
    )
    .unwrap();

    let backend = MockBackend::new([json!({"tool":"finish"}).to_string()]);
    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let cfg = AgentConfig {
        focus_files: vec!["app.py".to_string()],
        verbose: true,
        ..AgentConfig::default()
    };
    run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "edit app.py",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let evs = events.lock().unwrap();
    let prompt = evs
        .iter()
        .find_map(|e| match e {
            AgentEvent::PromptAssembled { messages, .. } => Some(
                messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .expect("a verbose run emits the assembled prompt");

    // The focused file's full body IS shown.
    assert!(
        prompt.contains("def handler():"),
        "focus file body must be pinned"
    );
    // The IMPORTED file (store) is shown IN FULL — app.py does `from store import add`.
    assert!(
        prompt.contains("IMPORTED_BODY_MARKER"),
        "an imported file's full body must be pinned (the model needs its code): {prompt}"
    );
    // The UNIMPORTED file (util) is NOT pinned in full — only its signature appears.
    assert!(
        !prompt.contains("UNIMPORTED_BODY_MARKER"),
        "an unimported file's body must NOT be pinned (signature only): {prompt}"
    );
    assert!(
        prompt.contains("util.py:"),
        "the unimported file appears as a signature"
    );
    // The prompt frames the imported files as read-only context.
    assert!(prompt.contains("IMPORTS FROM"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_batched_turn_writes_every_distinct_file_in_one_turn() {
    // Thread 3: the model emits the whole app as several create/write calls in ONE turn.
    // The loop must apply ALL the distinct-path writes that turn (not just the first and
    // discard the rest), then finish. Three files must exist after a single build turn.
    let ws = temp_dir("batch");
    let batched = "{\"tool\":\"create_file\",\"path\":\"store.py\",\"content\":\"S\"}\
                   {\"tool\":\"create_file\",\"path\":\"app.py\",\"content\":\"A\"}\
                   {\"tool\":\"write_file\",\"path\":\"util.py\",\"content\":\"U\"}";
    let backend = MockBackend::new([batched.to_string(), json!({"tool":"finish"}).to_string()]);
    let report = run_agent(&backend, "build the app", &ws, &AgentConfig::default()).unwrap();
    assert!(report.finished);
    // All three files written in the single batched turn (turn 1), finish on turn 2.
    assert_eq!(std::fs::read_to_string(ws.join("store.py")).unwrap(), "S");
    assert_eq!(std::fs::read_to_string(ws.join("app.py")).unwrap(), "A");
    assert_eq!(std::fs::read_to_string(ws.join("util.py")).unwrap(), "U");
    assert_eq!(report.steps, 2, "one batched build turn + finish");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn verbose_emits_the_assembled_prompt_only_when_enabled() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let registry = sc_tools::default_registry();

    let run = |verbose: bool| -> Vec<AgentEvent> {
        let ws = temp_dir(if verbose { "verbose-on" } else { "verbose-off" });
        let backend = MockBackend::new([json!({"tool":"finish"}).to_string()]);
        let strategy = crate::strategy::select_strategy(&backend.capabilities());
        let evs: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
        let sink = crate::event::FnSink(|e: &AgentEvent| evs.lock().unwrap().push(e.clone()));
        let cfg = AgentConfig {
            verbose,
            ..Default::default()
        };
        run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            "x",
            &ws,
            &cfg,
            &sink,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&ws);
        evs.into_inner().unwrap()
    };

    // Verbose on: a PromptAssembled event carries the real system prompt content.
    let on = run(true);
    let prompt = on.iter().find_map(|e| match e {
        AgentEvent::PromptAssembled { messages, .. } => Some(messages.clone()),
        _ => None,
    });
    let messages = prompt.expect("verbose run should emit PromptAssembled");
    assert!(
        messages.iter().any(|m| m.role == "system"),
        "the assembled prompt includes the system message: {messages:?}"
    );

    // Verbose off (default): no PromptAssembled events at all.
    let off = run(false);
    assert!(
        !off.iter()
            .any(|e| matches!(e, AgentEvent::PromptAssembled { .. })),
        "no prompt dump without --verbose"
    );
}

#[test]
fn dry_run_previews_mutations_without_touching_the_workspace() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("dry-run");
    std::fs::write(ws.join("f.txt"), "ORIGINAL").unwrap();

    // Turn 1: read the file (read-only — must run for real so the model sees it).
    // Turn 2: try to overwrite it (mutating — must be previewed, not applied).
    // Turn 3: finish.
    let backend = MockBackend::new([
        json!({"tool":"read_file","path":"f.txt"}).to_string(),
        json!({"tool":"write_file","path":"f.txt","content":"CLOBBERED"}).to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);

    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let cfg = AgentConfig {
        dry_run: true,
        ..Default::default()
    };
    let report = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "edit f.txt",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();
    assert!(report.finished);

    // The mutating tool never wrote: the file is byte-for-byte the original.
    assert_eq!(
        std::fs::read_to_string(ws.join("f.txt")).unwrap(),
        "ORIGINAL"
    );

    let evs = events.lock().unwrap();
    // The read returned the *real* content (read-only tools still run).
    assert!(
        evs.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { full, .. } if full.contains("ORIGINAL")
        )),
        "read_file should return the real file body in dry-run: {evs:?}"
    );
    // The write produced a [dry-run] preview note instead of applying.
    assert!(
        evs.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { summary, .. } if summary.contains("[dry-run]")
        )),
        "write_file should be previewed with a [dry-run] note: {evs:?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn recovers_from_a_malformed_tool_call() {
    let ws = temp_dir("repair");
    // First turn is garbage; the loop must feed back an error and continue.
    let backend = MockBackend::new([
        "not json at all".to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);

    let report = run_agent(&backend, "do it", &ws, &AgentConfig::default()).unwrap();
    assert!(report.finished);
    assert_eq!(report.steps, 2);
    // One invalid (the garbage), one valid (the finish).
    assert_eq!(report.metrics.invalid, 1);
    assert_eq!(report.metrics.valid, 1);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_schema_violation_is_repaired_not_executed() {
    let ws = temp_dir("schema-repair");
    // read_file without a path is valid JSON but invalid against the schema;
    // it must be fed back, not executed, then the model recovers.
    let backend = MockBackend::new([
        json!({"tool":"read_file"}).to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);
    let report = run_agent(&backend, "x", &ws, &AgentConfig::default()).unwrap();
    assert!(report.finished);
    assert_eq!(report.metrics.invalid, 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn stops_at_the_step_budget() {
    let ws = temp_dir("budget");
    // A backend that never finishes: always asks to read the same file.
    let read = json!({"tool":"read_file","path":"x"}).to_string();
    let backend = scripted_backend("looper", move |_req| {
        Ok(GenerateResponse::new(read.clone()))
    });

    let cfg = AgentConfig {
        max_steps: 3,
        ..Default::default()
    };
    let report = run_agent(&backend, "loop forever", &ws, &cfg).unwrap();
    assert!(!report.finished);
    assert_eq!(report.steps, 3);
    assert_eq!(report.metrics.valid, 3);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn propagates_backend_errors() {
    let ws = temp_dir("err");
    let backend = MockBackend::new(Vec::<String>::new()); // exhausts immediately
    assert!(run_agent(&backend, "x", &ws, &AgentConfig::default()).is_err());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn no_advisor_self_recovers_before_giving_up() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("self-recover");
    std::fs::write(ws.join("f.txt"), "BODY").unwrap();

    // A model that loops on the same read forever, with NO advisor. The harness
    // must steer it back in-band (emit Advice) at each stall instead of stopping
    // on the first one — but still terminate once the recovery budget is spent.
    let read = json!({"tool":"read_file","path":"f.txt"}).to_string();
    let backend = scripted_backend("self-recover-looper", move |_req| {
        Ok(GenerateResponse::new(read.clone()))
    });

    #[derive(Default)]
    struct Adv {
        advice: Mutex<Vec<String>>,
        stalled: Mutex<usize>,
    }
    impl crate::event::EventSink for Adv {
        fn record(&self, e: &AgentEvent) {
            match e {
                AgentEvent::Advice { advice, .. } => {
                    self.advice.lock().unwrap().push(advice.clone())
                }
                AgentEvent::Stalled { .. } => *self.stalled.lock().unwrap() += 1,
                _ => {}
            }
        }
    }

    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = Adv::default();
    let cfg = AgentConfig {
        max_steps: 30,
        ..Default::default()
    };
    let report = run_agent_observed(
        &backend,
        None, // no advisor — the single-model setup
        &registry,
        strategy.as_ref(),
        "read forever",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    // It eventually gives up (the model never edits), but only AFTER self-recovery.
    assert!(!report.finished);
    assert!(
        matches!(report.stop_reason, StopReason::Stalled(_)),
        "should stop stalled, got {:?}",
        report.stop_reason
    );
    // SELF_RECOVERY_LIMIT firm directives were injected before giving up.
    let advice = sink.advice.lock().unwrap();
    assert_eq!(
        advice.len(),
        SELF_RECOVERY_LIMIT,
        "expected {SELF_RECOVERY_LIMIT} self-recovery directives, got {advice:?}"
    );
    assert!(
        advice[0].contains("stuck in a loop") && advice[0].contains("edit_file"),
        "directive names the loop and points at the edit: {:?}",
        advice[0]
    );
    // It did NOT die on the first stall: more stalls than the no-advisor stop
    // would have allowed (1).
    assert!(*sink.stalled.lock().unwrap() > 1);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn repeated_edit_miss_is_steered_to_write_file() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("edit-loop");
    // The file exists but does NOT contain the model's imagined anchor, so every
    // edit_file misses. After two misses the harness must steer it to write_file.
    std::fs::write(ws.join("app.py"), "x = 1\n").unwrap();

    let miss = json!({"tool":"edit_file","path":"app.py",
        "old_str":"return jsonify(x)","new_str":"return jsonify(x), 200"})
    .to_string();
    let backend = MockBackend::new([
        miss.clone(),
        miss.clone(),
        miss, // 3 misses
        json!({"tool":"finish"}).to_string(),
    ]);

    #[derive(Default)]
    struct Cap {
        advice: Mutex<Vec<String>>,
    }
    impl crate::event::EventSink for Cap {
        fn record(&self, e: &AgentEvent) {
            if let AgentEvent::Advice { advice, .. } = e {
                self.advice.lock().unwrap().push(advice.clone());
            }
        }
    }

    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = Cap::default();
    let _ = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    let advice = sink.advice.lock().unwrap();
    assert!(
        advice.iter().any(|a| a.contains("write_file")
            && a.contains("anchor does not exist")
            && a.contains("app.py")),
        "a repeated edit miss must steer to write_file: {advice:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn repeated_create_file_clash_is_steered_to_write_file() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("create-loop");
    // app.py already exists. The model keeps calling create_file to "fix" it, but
    // create_file refuses to overwrite — so it would loop forever. After two clashes
    // the harness must steer it to write_file (observed live: the multi-file db task).
    std::fs::write(ws.join("app.py"), "x = 1\n").unwrap();

    let clash = json!({"tool":"create_file","path":"app.py","content":"y = 2\n"}).to_string();
    let backend = MockBackend::new([
        clash.clone(),
        clash.clone(),
        clash,
        json!({"tool":"finish"}).to_string(),
    ]);

    #[derive(Default)]
    struct Cap {
        advice: Mutex<Vec<String>>,
    }
    impl crate::event::EventSink for Cap {
        fn record(&self, e: &AgentEvent) {
            if let AgentEvent::Advice { advice, .. } = e {
                self.advice.lock().unwrap().push(advice.clone());
            }
        }
    }

    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = Cap::default();
    let _ = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    let advice = sink.advice.lock().unwrap();
    assert!(
        advice.iter().any(|a| a.contains("write_file")
            && a.contains("already exists")
            && a.contains("app.py")),
        "a repeated create_file clash must steer to write_file: {advice:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// The `ToolNotOffered` detector, both directions.
///
/// Driven through the helper rather than a contrived prompt, because the real
/// prompts are now correct -- `system_preamble` and `render_focus_files` both take
/// the registry. This pins the detector itself so it still works when the next piece
/// of guidance forgets to ask.
#[test]
fn unoffered_tool_detector_fires_only_on_harness_text_naming_a_missing_tool() {
    use sc_model::{Message, Role};

    let full = sc_tools::default_registry();
    let trimmed = sc_tools::ToolRegistry::new(
        full.specs()
            .iter()
            .filter(|s| s.name != "edit_lines")
            .cloned()
            .collect(),
    );

    // Harness text steering toward a tool that is gone: a fault.
    let steering = vec![Message::system(
        "To change a large file, PREFER `edit_lines`.",
    )];
    assert_eq!(
        unoffered_tool_mentioned(&steering, &trimmed).as_deref(),
        Some("edit_lines")
    );

    // The same text when the tool IS offered: not a fault.
    assert_eq!(unoffered_tool_mentioned(&steering, &full), None);

    // A USER instruction naming it is not our bug -- the user may say anything.
    let user_said = vec![Message::user("please use `edit_lines` for this")];
    assert_eq!(unoffered_tool_mentioned(&user_said, &trimmed), None);

    // A bare word without backticks is ordinary English, not a tool reference.
    // Firing here would put a fault on nearly every prompt.
    let prose = vec![Message::system("Edit lines carefully and then finish.")];
    assert_eq!(unoffered_tool_mentioned(&prose, &trimmed), None);

    // A name that is not a tool at all is never a fault.
    let unrelated = vec![Message::system("see `Cargo.toml` and `README.md`")];
    assert_eq!(unoffered_tool_mentioned(&unrelated, &trimmed), None);

    let _ = Role::System;
}

/// A snake_case tool name counts BARE, not only in backticks.
///
/// The live miss: `sc-verify`'s compile checklist ended "(edit_function for a match arm /
/// body)" and led every failed-verification observation, so on a six-tool run the model was
/// told to call a tool it did not have -- and this detector, matching only backticks, was
/// silent. `edit_function` is an identifier, never English, so bare matching is safe for it
/// while single words like `finish` still need backticks.
#[test]
fn a_bare_snake_case_tool_name_is_detected_but_a_bare_word_is_not() {
    use sc_model::Message;

    let six = registry_of(&[
        "read_file",
        "edit_file",
        "write_file",
        "run_command",
        "run_verification",
        "finish",
    ]);

    // THE regression: the exact string that shipped, unbackticked.
    let checklist = vec![Message::system(
        "COMPILE ERRORS to fix (1):\n  • a.rs:7 — mismatched types\n\
         Go to each file:line above and fix it (edit_function for a match arm / body).",
    )];
    assert_eq!(
        unoffered_tool_mentioned(&checklist, &six).as_deref(),
        Some("edit_function"),
        "a bare snake_case tool name must be caught"
    );

    // The exemption the detector was built around still holds: bare single words are
    // ordinary English and must not fire, or every prompt carries a fault.
    let prose = vec![Message::system(
        "Edit lines carefully, then finish and ask if unsure.",
    )];
    assert_eq!(unoffered_tool_mentioned(&prose, &six), None);

    // Whole-word only: a longer identifier that merely contains a tool name is not a
    // reference to that tool.
    let longer = vec![Message::system("see edit_function_helper in the notes")];
    assert_eq!(unoffered_tool_mentioned(&longer, &six), None);

    // And a tool the run DOES have is never a fault, bare or not.
    let offered = vec![Message::system("use edit_file for this")];
    assert_eq!(unoffered_tool_mentioned(&offered, &six), None);
}

/// A registry holding only the named built-in tools (plus nothing else).
fn registry_of(names: &[&str]) -> sc_tools::ToolRegistry {
    let full = sc_tools::default_registry();
    sc_tools::ToolRegistry::new(
        full.specs()
            .iter()
            .filter(|s| names.contains(&s.name))
            .cloned()
            .collect(),
    )
}

/// `mention` is the one way a directive may name a tool: best preference first, and
/// only ever a tool the registry has.
#[test]
fn mention_returns_the_first_preferred_tool_the_registry_offers() {
    use super::escalation::mention;
    let full = sc_tools::default_registry();
    assert_eq!(
        mention(&full, &["edit_lines", "edit_file"]),
        Some("edit_lines")
    );
    let no_lines = registry_of(&["read_file", "edit_file", "finish"]);
    assert_eq!(
        mention(&no_lines, &["edit_lines", "edit_file"]),
        Some("edit_file")
    );
    let read_only = registry_of(&["read_file", "finish"]);
    assert_eq!(mention(&read_only, &["edit_lines", "edit_file"]), None);
}

/// A directive is checked where it is injected, not only on the step-0 prompt scan.
#[test]
fn an_injected_directive_naming_an_unoffered_tool_is_a_fault_at_that_step() {
    use crate::event::{AgentEvent, FaultKind};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Faults(Mutex<Vec<(FaultKind, usize)>>);
    impl crate::event::EventSink for Faults {
        fn record(&self, e: &AgentEvent) {
            if let AgentEvent::HarnessFault { kind, step, .. } = e {
                self.0.lock().unwrap().push((*kind, *step));
            }
        }
    }

    let trimmed = registry_of(&["read_file", "write_file", "finish"]);
    let sink = Faults::default();
    report_if_unoffered("STOP. Use `edit_lines` now.", &trimmed, 6, &sink);
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![(FaultKind::ToolNotOffered, 7)],
        "names the missing tool on the turn it was injected"
    );

    // The same text against a registry that has it: silent.
    let sink = Faults::default();
    report_if_unoffered(
        "STOP. Use `edit_lines` now.",
        &sc_tools::default_registry(),
        6,
        &sink,
    );
    assert!(sink.0.lock().unwrap().is_empty());
}

/// The self-recovery directive is built from the registry: it names only tools the run
/// has, and on a read-only run it tells the model to answer rather than to edit.
#[test]
fn self_recovery_directive_never_names_a_tool_the_run_cannot_call() {
    use super::escalation::self_recovery_directive;
    let recent = vec!["read_file".to_string()];

    let full = self_recovery_directive(&recent, &sc_tools::default_registry(), 0, false);
    assert!(full.contains("`write_file`") && full.contains("`edit_file`"));
    assert!(full.contains("`run_verification`"));

    let six = registry_of(&["read_file", "write_file", "edit_lines", "finish"]);
    let d = self_recovery_directive(&recent, &six, 0, false);
    assert!(
        d.contains("`write_file`") && d.contains("`edit_lines`"),
        "{d}"
    );
    assert!(
        !d.contains("`edit_file`") && !d.contains("`run_verification`"),
        "{d}"
    );
    assert_eq!(unoffered_tool_in(&d, &six), None);

    let read_only = registry_of(&["read_file", "finish"]);
    let d = self_recovery_directive(&recent, &read_only, 0, false);
    assert!(d.contains("`finish`"), "{d}");
    assert_eq!(unoffered_tool_in(&d, &read_only), None, "{d}");
}

/// THE defect: the directive recommended and forbade the SAME tool in one sentence.
///
/// A model looping on `edit_file` against the full registry was handed "Emit `write_file`
/// or `edit_file` (an action that changes the workspace) this turn. Do NOT emit `edit_file`
/// again." Measured over one Mellum run this fired 8 times, and the model obeyed the
/// prohibition and dropped the recommendation more often than the reverse -- 3 of the 8
/// next turns were the do-nothing calls the nudge exists to prevent.
///
/// The rule is absolute, and asserted on the exact strings the model reads: whatever tool
/// the directive recommends, it must not go on to forbid.
#[test]
fn self_recovery_directive_never_recommends_and_forbids_the_same_tool() {
    use super::escalation::self_recovery_directive;

    let full = sc_tools::default_registry();
    let d = self_recovery_directive(&["edit_file".to_string()], &full, 0, false);
    assert!(
        !d.contains("Emit `write_file` or `edit_file`"),
        "recommends the looped tool it then forbids: {d}"
    );
    assert!(
        !d.contains("use `edit_file` for a small"),
        "the body bullet still recommends the looped tool: {d}"
    );
    // The prohibition survives, and there is still a legal concrete move.
    assert!(d.contains("Do NOT emit `edit_file` again."), "{d}");
    assert!(
        d.contains("Emit `write_file`"),
        "no concrete move left: {d}"
    );

    // The same must hold for every edit tool in the registry, in both directions.
    for looped in ["write_file", "edit_file"] {
        let d = self_recovery_directive(&[looped.to_string()], &full, 0, false);
        let forbids = d.contains(&format!("Do NOT emit `{looped}` again"));
        let recommends = d.contains(&format!("Emit `{looped}`"))
            || d.contains(&format!("or `{looped}`"))
            || d.contains(&format!("with `{looped}`"))
            || d.contains(&format!("use `{looped}`"));
        assert!(
            !(forbids && recommends),
            "`{looped}` is both recommended and forbidden: {d}"
        );
    }
}

/// Excluding the looped tool can empty the recommendation list: a registry with exactly one
/// edit tool, and the model looping on it. "Do not use it again" is then wrong advice --
/// nothing else here can change the workspace -- so the directive must say how to use that
/// tool DIFFERENTLY, and must never forbid it.
#[test]
fn self_recovery_directive_on_the_only_edit_tool_says_how_to_use_it_differently() {
    use super::escalation::self_recovery_directive;

    for (tools, looped) in [
        (&["read_file", "edit_file", "finish"][..], "edit_file"),
        (&["read_file", "write_file", "finish"][..], "write_file"),
    ] {
        let reg = registry_of(tools);
        let d = self_recovery_directive(&[looped.to_string()], &reg, 0, false);
        assert!(
            !d.contains(&format!("Do NOT emit `{looped}`")),
            "forbids the only edit tool: {d}"
        );
        assert!(
            d.contains(&format!(
                "Emit `{looped}` this turn with DIFFERENT arguments."
            )),
            "no concrete move named: {d}"
        );
        assert_eq!(unoffered_tool_in(&d, &reg), None, "{d}");
    }
}

/// `recent_edit_path` finds the file the loop is actually about: the most recent MUTATING
/// turn's arg, never a read's.
///
/// The distinction is load-bearing. A `read_file` arg is a path too, but the file the model
/// last READ is not necessarily the one it is failing to WRITE, and sizing the wrong file is
/// how the deadlock this feeds would come back wearing a different hat.
#[test]
fn recent_edit_path_finds_the_last_mutated_file_not_the_last_read() {
    use super::escalation::recent_edit_path;
    use sc_context::TurnRecord;

    let h = vec![
        TurnRecord::new("edit_file", "src/world.rs", true),
        TurnRecord::new("read_file", "tests/contract.rs", false),
        TurnRecord::new("run_verification", "", true),
    ];
    assert_eq!(recent_edit_path(&h), Some("src/world.rs"));

    // Most recent mutation wins.
    let h = vec![
        TurnRecord::new("edit_file", "a.rs", true),
        TurnRecord::new("write_file", "b.rs", false),
    ];
    assert_eq!(recent_edit_path(&h), Some("b.rs"));

    // Reads only, or an empty arg: nothing to size.
    let h = vec![TurnRecord::new("read_file", "a.rs", false)];
    assert_eq!(recent_edit_path(&h), None);
    let h = vec![TurnRecord::new("edit_file", "", true)];
    assert_eq!(recent_edit_path(&h), None);
    assert_eq!(recent_edit_path(&[]), None);
}

/// **THE CROSS-GUARD DEADLOCK: never steer at `write_file` for a file it will refuse.**
///
/// `write.rs`'s own doc comment on `WRITE_FILE_OVERWRITE_MAX_LINES` predicted this -- "telling
/// it to rewrite a file this guard will then refuse is a deadlock" -- and the failed-edit path
/// in `mod.rs` asks (`rewrite_target`). The stall ladder never did.
///
/// Measured on `engine-ecs-query`, run 3, one turn apart:
///
/// ```text
///   step 27  advice: Emit `write_file` … Do NOT emit `edit_file` again.
///   step 28  result: write_file src/world.rs rejected: … 276 lines — too large … Use edit_file
/// ```
///
/// Both moves closed. With `oversize_target`, `write_file` leaves the recommendations, which
/// on a six-tool `edit_file` loop empties both lists and routes into the `only_edit_tool`
/// branch -- keep using the one tool that can work, with DIFFERENT arguments.
#[test]
fn self_recovery_directive_does_not_steer_at_write_file_for_an_oversize_file() {
    use super::escalation::self_recovery_directive;

    let six = registry_of(&[
        "read_file",
        "edit_file",
        "write_file",
        "run_command",
        "run_verification",
        "finish",
    ]);
    let looped = vec!["edit_file".to_string()];

    // Baseline: a normal-sized target still offers the wholesale rewrite.
    let ok = self_recovery_directive(&looped, &six, 0, false);
    assert!(
        ok.contains("`write_file`"),
        "a small file may still be rewritten: {ok}"
    );

    // Oversize: `write_file` must not be named as a way out, and `edit_file` -- the only
    // tool left that can change anything -- must NOT be forbidden.
    let d = self_recovery_directive(&looped, &six, 0, true);
    assert!(
        !d.contains("`write_file`"),
        "steers at a rewrite the guard will refuse: {d}"
    );
    assert!(
        !d.contains("Do NOT emit `edit_file`"),
        "forbids the only tool that can still work: {d}"
    );
    assert!(
        d.contains("Emit `edit_file` this turn with DIFFERENT arguments."),
        "must land in the only_edit_tool branch with a concrete move: {d}"
    );
    assert_eq!(unoffered_tool_in(&d, &six), None, "{d}");
}

/// The read-only arm carried the same contradiction latently: it tells the model to call
/// `finish`, then appends "Do NOT emit {looped} again" -- which reads as "call `finish` NOW
/// ... do NOT emit `finish` again" when `finish` is the tool being looped on.
#[test]
fn self_recovery_directive_looping_on_finish_does_not_forbid_finish() {
    use super::escalation::self_recovery_directive;

    let read_only = registry_of(&["read_file", "finish"]);
    let d = self_recovery_directive(&["finish".to_string()], &read_only, 0, false);
    assert!(d.contains("call `finish` NOW"), "{d}");
    assert!(
        !d.contains("Do NOT emit `finish`"),
        "recommends and forbids `finish`: {d}"
    );
    // Still a concrete move: what to do differently with the tool it must keep using.
    assert!(d.contains("`summary`"), "{d}");

    // The normal read-only case is untouched: looping on `read_file` still forbids it.
    let d = self_recovery_directive(&["read_file".to_string()], &read_only, 0, false);
    assert!(d.contains("Do NOT emit `read_file` again."), "{d}");
}

/// The correct behaviour must survive the fix: a model looping on a READ tool against a full
/// registry still gets both edit tools recommended and the read tool forbidden.
#[test]
fn self_recovery_directive_keeps_the_normal_case_intact() {
    use super::escalation::self_recovery_directive;

    let d = self_recovery_directive(
        &["read_file".to_string()],
        &sc_tools::default_registry(),
        0,
        false,
    );
    assert!(
        d.contains(
            "Emit `write_file` or `edit_file` (an action that changes the workspace) \
             this turn. Do NOT emit `read_file` again."
        ),
        "{d}"
    );
    assert!(d.contains("use `edit_file` for a small"), "{d}");
    assert!(d.contains("then `run_verification`"), "{d}");
}

/// "You already have everything you read in the context above" is only true while nothing
/// has been evicted. Once older turns are compacted into the summary the model may genuinely
/// NOT have what it read, and a harness that asserts something the model can see is false
/// teaches it to distrust the rest of the directive.
#[test]
fn self_recovery_directive_softens_the_you_have_everything_claim_after_eviction() {
    use super::escalation::self_recovery_directive;
    let recent = vec!["read_file".to_string()];
    let reg = sc_tools::default_registry();

    let fresh = self_recovery_directive(&recent, &reg, 0, false);
    assert!(
        fresh.contains("You already have everything you read"),
        "{fresh}"
    );

    let evicted = self_recovery_directive(&recent, &reg, 3, false);
    assert!(
        !evicted.contains("You already have everything you read"),
        "claims something false after eviction: {evicted}"
    );
    assert!(evicted.contains("compacted into the summary"), "{evicted}");
    // The actionable half is unchanged either way.
    assert!(
        evicted.contains("Do NOT emit `read_file` again."),
        "{evicted}"
    );
}

/// `ask_user` and the stall ladder spend ONE advisor budget between them. Before, only
/// the ladder checked `ADVISOR_LIMIT`; a model could `ask_user` the senior forever.
#[test]
fn ask_user_shares_the_advisor_budget_with_the_stall_ladder() {
    use super::escalation::ADVISOR_LIMIT;
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("ask-budget");
    let ask = json!({"tool":"ask_user","question":"which file?"}).to_string();
    let mut turns = vec![ask; ADVISOR_LIMIT + 1];
    turns.push(json!({"tool":"finish"}).to_string());
    let backend = MockBackend::new(turns);
    // The advisor has exactly ADVISOR_LIMIT answers in it; a further consult would exhaust
    // it and error the run, so a green run proves the limit gated the last ask.
    let advisor = MockBackend::new(vec!["Look in app.py.".to_string(); ADVISOR_LIMIT]);

    #[derive(Default)]
    struct Rec(Mutex<Vec<String>>);
    impl crate::event::EventSink for Rec {
        fn record(&self, e: &AgentEvent) {
            if let AgentEvent::Advice { advice, .. } = e {
                self.0.lock().unwrap().push(advice.clone());
            }
        }
    }
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = Rec::default();
    let cfg = AgentConfig {
        max_steps: 10,
        repeat_limit: 99,
        no_progress_limit: 99,
        ..Default::default()
    };
    let report = run_agent_observed(
        &backend,
        Some(&advisor),
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    assert!(report.finished, "{:?}", report.stop_reason);
    assert_eq!(report.interventions, ADVISOR_LIMIT + 1);
    let advice = sink.0.lock().unwrap();
    assert_eq!(advice.len(), ADVISOR_LIMIT + 1);
    assert!(
        advice.last().unwrap().contains("No one is available"),
        "past the budget the model is told to decide for itself: {advice:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A sink that captures every `Advice` directive the loop injects.
#[derive(Default)]
struct AdviceSink(std::sync::Mutex<Vec<String>>);

impl crate::event::EventSink for AdviceSink {
    fn record(&self, e: &crate::event::AgentEvent) {
        if let crate::event::AgentEvent::Advice { advice, .. } = e {
            self.0.lock().unwrap().push(advice.clone());
        }
    }
}

/// Drive `n` identical anchor misses on `path` and return the directives the loop injected.
fn miss_directives(ws: &Path, path: &str, n: usize) -> Vec<String> {
    let miss = json!({"tool":"edit_file","path":path,
        "old_str":"fn nothing_like_this_is_in_the_file()","new_str":"fn other()"})
    .to_string();
    let mut script: Vec<String> = vec![miss; n];
    script.push(json!({"tool":"finish"}).to_string());
    let backend = MockBackend::new(script);
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = AdviceSink::default();
    let _ = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();
    let out = sink.0.lock().unwrap().clone();
    out
}

/// **The rewrite escalation carries the WHOLE file, byte for byte.**
///
/// THE BUG. On the `engine-grid-scan` rung the escalation said "call `write_file` ... Base it
/// on the file shown in the error above". The only thing shown above was `anchor_not_found`'s
/// bounded 30-line window -- of a 94-line `floor.rs`, cut mid-line 41. The model obeyed
/// literally: it reconstructed 94 lines from 30, collapsed `//!` to `!` on line 2 and
/// truncated the rest. The resulting `expected item, found bang` ate 26 of the 40 turns.
///
/// The window is right for "here is where your anchor nearly matched" and wrong as the basis
/// for a whole-file rewrite. If the harness asks for a rewrite, the harness supplies the file.
#[test]
fn the_rewrite_escalation_carries_the_whole_file_not_a_window() {
    let ws = temp_dir("rewrite-full-file");
    // The rung's file in miniature: longer than MISS_MAX_LINES (30) so a window could never
    // carry it, and under the write_file overwrite cap so a rewrite is genuinely the steer.
    let mut src = String::from("//! The floor.\n//! Second doc line - the one that got mangled.\n");
    for n in 1..=92 {
        src.push_str(&format!("pub const V{n}: u32 = {n};\n"));
    }
    assert_eq!(src.lines().count(), 94);
    std::fs::write(ws.join("floor.rs"), &src).unwrap();

    let advice = miss_directives(&ws, "floor.rs", 3);
    let d = advice
        .iter()
        .find(|a| a.contains("write_file"))
        .unwrap_or_else(|| panic!("a repeated miss must steer to write_file: {advice:?}"));

    // It no longer points at something it did not show.
    assert!(
        !d.contains("shown in the error above"),
        "the harness must not call the window the file: {d}"
    );
    // Every line of the file is present, numbered, in order -- including line 2, the one the
    // model mangled, and line 94, well past where the window stopped.
    for (i, line) in src.lines().enumerate() {
        assert!(
            d.contains(&format!("\n{}: {line}", i + 1)),
            "line {} of the file is missing from the directive",
            i + 1
        );
    }
    assert!(
        d.contains("2: //! Second doc line"),
        "the doc-comment line that got mangled must be there verbatim: {d}"
    );
    assert!(
        d.contains("(94 lines)"),
        "the directive names the size: {d}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **Too large to rewrite: the escalation steers surgical and never asks for a rewrite.**
///
/// `write_file` REFUSES to overwrite a file above `WRITE_FILE_OVERWRITE_MAX_LINES`, so a
/// directive steering there is a deadlock -- the model is told to do the one thing the next
/// guard rejects. The escalation asks the SAME constant, so the two cannot disagree.
#[test]
fn a_file_too_large_to_rewrite_is_steered_to_a_surgical_tool() {
    let ws = temp_dir("rewrite-too-large");
    let n = sc_tools::WRITE_FILE_OVERWRITE_MAX_LINES + 1;
    let src: String = (1..=n)
        .map(|i| format!("pub const V{i}: u32 = {i};\n"))
        .collect();
    std::fs::write(ws.join("big.rs"), &src).unwrap();

    let advice = miss_directives(&ws, "big.rs", 3);
    let d = advice
        .iter()
        .find(|a| a.contains("big.rs"))
        .unwrap_or_else(|| panic!("a repeated miss must be escalated: {advice:?}"));

    assert!(
        d.contains("edit_function") || d.contains("edit_lines"),
        "a big file must be steered at a surgical tool: {d}"
    );
    assert!(
        !d.contains("ENTIRE corrected file contents") && !d.contains("ENTIRE new file contents"),
        "it must NOT ask for a rewrite write_file would refuse: {d}"
    );
    assert!(
        d.contains(&format!("({n} lines)")),
        "the directive names the size that rules a rewrite out: {d}"
    );
    // And it must not inline the file it just called too big to reproduce.
    assert!(
        !d.contains("in full, as it is on disk right now"),
        "no whole-file dump for a file that cannot be rewritten: {d}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **One miss is still just the bounded window.** The escalation fires on a STREAK; a single
/// anchor miss must cost exactly what it always cost -- the lines around the closest match, no
/// directive and no file. Bloating every miss with a whole file is the other way to lose the
/// model's window.
#[test]
fn a_single_anchor_miss_still_shows_only_the_bounded_window() {
    let ws = temp_dir("single-miss-window");
    let src: String = (1..=94)
        .map(|i| format!("pub const V{i}: u32 = {i};\n"))
        .collect();
    std::fs::write(ws.join("floor.rs"), &src).unwrap();

    #[derive(Default)]
    struct Cap {
        advice: std::sync::Mutex<Vec<String>>,
        results: std::sync::Mutex<Vec<String>>,
    }
    impl crate::event::EventSink for Cap {
        fn record(&self, e: &crate::event::AgentEvent) {
            match e {
                crate::event::AgentEvent::Advice { advice, .. } => {
                    self.advice.lock().unwrap().push(advice.clone())
                }
                crate::event::AgentEvent::ToolResult { full, .. } => {
                    self.results.lock().unwrap().push(full.clone())
                }
                _ => {}
            }
        }
    }

    // One miss, then finish: the streak never reaches 2.
    let backend = MockBackend::new([
        json!({"tool":"edit_file","path":"floor.rs",
               "old_str":"fn nothing_like_this_is_in_the_file()","new_str":"fn other()"})
        .to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);
    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = Cap::default();
    let _ = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    assert!(
        sink.advice.lock().unwrap().is_empty(),
        "one miss is not a streak: {:?}",
        sink.advice.lock().unwrap()
    );
    let results = sink.results.lock().unwrap();
    let miss = results
        .iter()
        .find(|r| r.contains("anchor not found"))
        .unwrap_or_else(|| panic!("expected an anchor miss: {results:?}"));
    assert!(
        !miss.contains("in full, as it is on disk right now"),
        "an ordinary miss must not carry the file: {miss}"
    );
    assert!(
        miss.lines().count() <= 31,
        "header plus at most MISS_MAX_LINES, got {}: {miss}",
        miss.lines().count()
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **The inlined file must SURVIVE the observation cap.**
///
/// `edit_file` normally takes the tight `observation_line_cap` -- right for a status line,
/// fatal for one that now holds a whole file. The DEFAULT cap (200) happens to clear the
/// largest inlinable file (150) plus its miss window today, but `observation_line_cap` is
/// configurable: lower it and `truncate_observation` slices the very file the directive just
/// told the model to reproduce -- the original bug wearing a new hat. So an observation that
/// carries a file is capped like a read, and this pins that with a cap that would cut it.
#[test]
fn the_inlined_file_survives_the_observation_cap() {
    let ws = temp_dir("rewrite-cap");
    // The largest file the rewrite steer will ever inline.
    let n = sc_tools::WRITE_FILE_OVERWRITE_MAX_LINES;
    let src: String = (1..=n)
        .map(|i| format!("pub const V{i}: u32 = {i};\n"))
        .collect();
    std::fs::write(ws.join("max.rs"), &src).unwrap();

    let cfg = AgentConfig {
        // Well under the file: without the read cap this observation would be sliced.
        observation_line_cap: 40,
        ..AgentConfig::default()
    };
    assert!(
        n > cfg.observation_line_cap,
        "the file must outgrow the tight cap for this to test anything"
    );

    // Record what the model is actually SENT, which is the only thing that matters here.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let calls = std::sync::Mutex::new(0usize);
    let recorder = seen.clone();
    let miss = json!({"tool":"edit_file","path":"max.rs",
        "old_str":"fn nothing_like_this_is_in_the_file()","new_str":"fn other()"})
    .to_string();
    let backend = scripted_backend("cap", move |req: &sc_model::GenerateRequest| {
        recorder
            .lock()
            .unwrap()
            .extend(req.messages.iter().map(|m| m.content.clone()));
        let mut c = calls.lock().unwrap();
        *c += 1;
        let content = if *c <= 2 {
            miss.clone()
        } else {
            json!({"tool":"finish"}).to_string()
        };
        Ok(GenerateResponse {
            content,
            ..Default::default()
        })
    });

    let registry = sc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = NullSink;
    let _ = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let msgs = seen.lock().unwrap();
    let with_file = msgs
        .iter()
        .find(|m| m.contains("in full, as it is on disk right now"))
        .unwrap_or_else(|| panic!("the rewrite steer never reached the model"));
    // Both ends of the file survived the cap.
    assert!(
        with_file.contains("1: pub const V1: u32 = 1;"),
        "head kept: {with_file}"
    );
    assert!(
        with_file.contains(&format!("{n}: pub const V{n}: u32 = {n};")),
        "tail kept -- a cut file is exactly the bug this fix exists for"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
