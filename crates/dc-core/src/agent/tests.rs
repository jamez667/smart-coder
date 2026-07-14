//! Integration tests for the agent loop (`run_agent_observed` and friends).
//!
//! These drive the whole act→observe→recover cycle end to end; the narrower unit tests
//! for the extracted helpers live beside those helpers (see `dispatch`, `prompt`, `window`).

use super::*;
use dc_model::{CallbackBackend, GenerateResponse, MockBackend};
use serde_json::json;

use super::escalation::{DIAGNOSIS_LIMIT, SELF_RECOVERY_LIMIT};
use super::test_util::temp_dir;

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
    let caps = dc_model::Capabilities {
        max_context_tokens: 8192,
        tool_calling: dc_model::ToolCalling::None,
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
        Ok(GenerateResponse { content })
    });

    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = dc_tools::default_registry();
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
fn no_diagnosis_when_the_flag_is_off_or_no_verify_command() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let run = |diagnose: bool, verify: Option<&str>| -> usize {
        let ws = temp_dir("no-diag");
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        // A backend that loops forever on a no-op read (so the run stalls), via a
        // callback (MockBackend errors once exhausted).
        let caps = dc_model::Capabilities {
            max_context_tokens: 8192,
            tool_calling: dc_model::ToolCalling::None,
            on_device: false,
        };
        let backend = CallbackBackend::new("looper", caps, |_req: &GenerateRequest| {
            Ok(GenerateResponse {
                content: r#"{"tool":"read_file","path":"a.txt"}"#.to_string(),
            })
        });
        let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
        let sink = crate::event::FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
        let registry = dc_tools::default_registry();
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
    let registry = dc_tools::default_registry();
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
    let registry = dc_tools::default_registry();
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

    let registry = dc_tools::default_registry();

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
    let registry = dc_tools::default_registry();
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
    let backend = CallbackBackend::android_core(move |_req| {
        Ok(GenerateResponse {
            content: read.clone(),
        })
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
fn a_repeated_read_is_nudged_not_re_served() {
    use crate::event::AgentEvent;
    use std::sync::Mutex;

    let ws = temp_dir("read-dedup");
    std::fs::write(ws.join("f.txt"), "FILE_BODY_MARKER").unwrap();

    // The model reads the same file twice, then finishes. The second read must
    // come back as a nudge — not the file body again.
    let backend = MockBackend::new([
        json!({"tool":"read_file","path":"f.txt"}).to_string(),
        json!({"tool":"read_file","path":"f.txt"}).to_string(),
        json!({"tool":"finish"}).to_string(),
    ]);

    #[derive(Default)]
    struct Rec {
        results: Mutex<Vec<String>>,
    }
    impl crate::event::EventSink for Rec {
        fn record(&self, e: &AgentEvent) {
            if let AgentEvent::ToolResult { full, .. } = e {
                self.results.lock().unwrap().push(full.clone());
            }
        }
    }

    let registry = dc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    let sink = Rec::default();
    let report = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "read it",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();
    assert!(report.finished);

    let results = sink.results.lock().unwrap();
    // First read returns the file body; the second is the de-dup nudge.
    assert!(
        results[0].contains("FILE_BODY_MARKER"),
        "first read serves the file: {:?}",
        results[0]
    );
    assert!(
        results[1].contains("already have the result") && !results[1].contains("FILE_BODY_MARKER"),
        "second identical read is nudged, not re-served: {:?}",
        results[1]
    );
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
    let backend = CallbackBackend::android_core(move |_req| {
        Ok(GenerateResponse {
            content: read.clone(),
        })
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

    let registry = dc_tools::default_registry();
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

    let registry = dc_tools::default_registry();
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

    let registry = dc_tools::default_registry();
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
