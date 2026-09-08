//! Tests for the A/B harness itself.
//!
//! A benchmark nobody has tested is a number-generator, not a measurement. These
//! check the properties that would silently invalidate any result the A/B
//! produces: that the two arms differ only where intended, that the seam is
//! inert when unused, and that `ask` actually reaches the gateway.

use std::path::Path;

use sc_core::ExternalTool;
use sc_eval::gateway_arm::{control_registry, gateway_registry, Arm, GatewayTool};
use sc_model::ModelBackend;

/// A validated `ask` call, built the way the loop would.
fn ask_call(need: &str, path: Option<&str>) -> sc_tools::ValidatedCall {
    let mut obj = serde_json::Map::new();
    obj.insert("tool".into(), "ask".into());
    obj.insert("need".into(), need.into());
    if let Some(p) = path {
        obj.insert("path".into(), p.into());
    }
    let registry = sc_tools::ToolRegistry::new(vec![GatewayTool::spec()]);
    registry
        .validate(&serde_json::Value::Object(obj))
        .expect("ask call validates")
}

// ---------------------------------------------------------------------------
// The two arms: same everything except how the model looks.
// ---------------------------------------------------------------------------

#[test]
fn the_arms_differ_only_in_how_the_model_looks() {
    let control: Vec<&str> = control_registry().specs().iter().map(|s| s.name).collect();
    let gateway: Vec<&str> = gateway_registry().specs().iter().map(|s| s.name).collect();

    // Everything that CHANGES the workspace, or checks it, is identical. A
    // difference in solve rate must not be attributable to unequal power.
    for tool in ["edit_file", "write_file", "run_verification", "finish"] {
        assert!(control.contains(&tool), "control lost {tool}");
        assert!(gateway.contains(&tool), "gateway lost {tool}");
    }

    // The reading half is the whole experiment.
    assert!(control.contains(&"read_file"));
    assert!(!gateway.contains(&"read_file"), "gateway kept read_file");
    assert!(gateway.contains(&"ask"));
    assert!(!control.contains(&"ask"), "control got the gateway tool");
}

#[test]
fn the_gateway_arm_has_no_shell() {
    // The one deliberate asymmetry, pinned so it stays deliberate. On the
    // control arm `run_command` is the investigation tool; on the gateway arm
    // that job is `ask`. Leaving both would measure a model with two ways to
    // look rather than two designs.
    let gateway: Vec<&str> = gateway_registry().specs().iter().map(|s| s.name).collect();
    assert!(!gateway.contains(&"run_command"), "gateway arm has a shell");
    let control: Vec<&str> = control_registry().specs().iter().map(|s| s.name).collect();
    assert!(
        control.contains(&"run_command"),
        "control arm lost its shell"
    );
}

#[test]
fn the_gateway_arm_is_not_a_bigger_menu() {
    // The premise under test is that a NARROWER surface helps. If the gateway
    // arm ever offered more tools than the control, the experiment would be
    // measuring the opposite of its own hypothesis.
    assert!(
        gateway_registry().specs().len() < control_registry().specs().len(),
        "the gateway arm is not narrower than the control"
    );
}

#[test]
fn neither_arm_can_reach_a_mutating_tool_through_ask() {
    // `ask` is read-only by construction. Pinned here too, because this test
    // fails loudly at the A/B level if the gateway's own guard ever regresses.
    assert_eq!(
        GatewayTool::spec().side_effect,
        sc_tools::SideEffect::ReadOnly
    );
}

// ---------------------------------------------------------------------------
// The seam: inert unless used.
// ---------------------------------------------------------------------------

#[test]
fn the_external_seam_declines_tools_it_does_not_own() {
    // Returning None must let the loop route normally. A seam that swallowed
    // unknown calls would break every built-in tool the moment it was attached.
    let tool = GatewayTool::new();
    let registry = sc_tools::default_registry();
    let call = registry
        .validate(&serde_json::json!({"tool": "read_file", "path": "src/lib.rs"}))
        .expect("read_file validates");
    assert!(
        tool.execute(&call, Path::new(".")).is_none(),
        "the gateway seam claimed a tool it does not own"
    );
}

#[test]
fn an_unattached_seam_leaves_the_config_untouched() {
    // The safety property for the live loop: nothing is wired in until an
    // experiment wires it.
    let cfg = sc_core::AgentConfig::default();
    assert!(
        cfg.external_tool.is_none(),
        "the default config now carries an external tool"
    );
}

// ---------------------------------------------------------------------------
// `ask` actually reaches the gateway, and reports what it did.
// ---------------------------------------------------------------------------

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn max_run(v: &[i64]) -> i64 {\n    v.iter().copied().max().unwrap_or(0)\n}\n",
    )
    .unwrap();
    dir
}

#[test]
fn ask_routes_a_plain_english_need_to_a_capability() {
    let dir = fixture();
    let tool = GatewayTool::new();
    let outcome = tool
        .execute(&ask_call("read src/lib.rs", None), dir.path())
        .expect("ask is owned by the gateway seam");

    match outcome {
        sc_tools::ToolOutcome::Observation(text) => {
            assert!(
                text.contains("max_run"),
                "ask did not return the file: {text}"
            )
        }
        sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
    }

    let stats = tool.stats();
    assert_eq!(stats.calls, 1);
    assert_eq!(stats.routed, 1);
    assert_eq!(stats.refused, 0);
    assert_eq!(stats.answer_percent(), 100);
}

#[test]
fn ask_honours_the_path_argument_as_a_scope() {
    // The harness supplies the path; the model supplies the question. Same rule
    // as everywhere else — the model does not compose paths.
    let dir = fixture();
    let tool = GatewayTool::new();
    let outcome = tool
        .execute(&ask_call("read the file", Some("src/lib.rs")), dir.path())
        .expect("owned");
    match outcome {
        sc_tools::ToolOutcome::Observation(text) => assert!(text.contains("max_run")),
        sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
    }
}

#[test]
fn a_refusal_is_counted_and_still_answers_the_model() {
    // A refusal must reach the model as usable text — it is the model's cue to
    // rephrase — and must be visible in the stats, because a solve rate earned
    // at a high refusal rate did not come from the gateway.
    let dir = fixture();
    let tool = GatewayTool::new();
    let outcome = tool
        .execute(&ask_call("do the thing", None), dir.path())
        .expect("owned");
    match outcome {
        sc_tools::ToolOutcome::Observation(text) => {
            assert!(
                text.starts_with("Cannot answer that"),
                "a refusal must read as one: {text}"
            )
        }
        sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
    }

    let stats = tool.stats();
    assert_eq!(stats.refused, 1);
    assert_eq!(stats.routed, 0);
    assert_eq!(stats.answer_percent(), 0);
}

#[test]
fn verification_is_reachable_through_ask() {
    // **The bug that invalidated the first A/B run.** `verify.run` was left
    // unwired on the grounds that live capabilities are unattributable, which
    // conflated "needs a seam" with "needs a model" — running tests spawns a
    // process and involves no model at all.
    //
    // The consequence was not a visible failure: the gateway arm simply refused
    // every "what is failing" while the control arm had a shell, the one
    // capability whose reduction works was never exercised, and the run reported
    // 100% retained. Nothing was red. This test is what would have caught it.
    let dir = fixture();
    let tool = GatewayTool::new();
    tool.arm_verification(sc_verify::Sandbox::Host, "exit 1");

    let outcome = tool
        .execute(&ask_call("what is failing", None), dir.path())
        .expect("owned");
    match outcome {
        sc_tools::ToolOutcome::Observation(text) => assert!(
            !text.starts_with("Cannot answer"),
            "the gateway arm cannot answer 'what is failing': {text}"
        ),
        sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
    }

    let stats = tool.stats();
    assert_eq!(stats.refused, 0, "a verification need was refused");
    assert!(
        stats.by_capability.iter().any(|(n, _)| n == "verify.run"),
        "verification did not route to verify.run: {:?}",
        stats.by_capability
    );
}

#[test]
fn the_phrasings_a_model_actually_types_are_answerable() {
    // The first run refused 14 of 50 calls, and every refused phrasing was a
    // variation on "what is failing". A gateway that cannot answer the question
    // an agent asks most is not being tested — it is being bypassed.
    let dir = fixture();
    let tool = GatewayTool::new();
    tool.arm_verification(sc_verify::Sandbox::Host, "exit 1");

    for need in ["run the tests", "what is failing", "are the tests passing"] {
        let outcome = tool
            .execute(&ask_call(need, None), dir.path())
            .expect("owned");
        match outcome {
            sc_tools::ToolOutcome::Observation(text) => assert!(
                !text.starts_with("Cannot answer"),
                "{need:?} was refused: {text}"
            ),
            sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
        }
    }
    assert_eq!(tool.stats().refused, 0);
}

#[test]
fn verification_refuses_when_no_command_is_armed() {
    // The counterpart: the gateway must never INVENT a test command. Unarmed, it
    // refuses rather than running something arbitrary and reporting its output
    // as the suite result.
    let dir = fixture();
    let tool = GatewayTool::new();
    let outcome = tool
        .execute(&ask_call("run the tests", None), dir.path())
        .expect("owned");
    match outcome {
        sc_tools::ToolOutcome::Observation(text) => {
            assert!(
                text.contains("Cannot answer"),
                "invented a test command: {text}"
            )
        }
        sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
    }
}

#[test]
fn live_capabilities_refuse_inside_the_ab() {
    // A live capability would put a SECOND model in the loop and make the
    // result unattributable. The seam supplies no model, so these refuse.
    let dir = fixture();
    let tool = GatewayTool::new();
    let outcome = tool
        .execute(&ask_call("explain why this uses a trait", None), dir.path())
        .expect("owned");
    match outcome {
        sc_tools::ToolOutcome::Observation(text) => {
            assert!(
                text.contains("Cannot answer"),
                "a live need was answered: {text}"
            )
        }
        sc_tools::ToolOutcome::Finished => panic!("ask returned Finished"),
    }
    assert_eq!(tool.stats().refused, 1);
}

#[test]
fn stats_accumulate_across_calls_and_name_the_capabilities() {
    let dir = fixture();
    let tool = GatewayTool::new();
    for need in ["read src/lib.rs", "read src/lib.rs", "do the thing"] {
        let _ = tool.execute(&ask_call(need, None), dir.path());
    }
    let stats = tool.stats();
    assert_eq!(stats.calls, 3);
    assert_eq!(stats.routed, 2);
    assert_eq!(stats.refused, 1);
    assert_eq!(
        stats.by_capability,
        vec![("file.read".to_string(), 2)],
        "capability usage was not tracked"
    );
    assert!(
        stats.raw_bytes > 0,
        "no bytes recorded for an answered call"
    );
}

#[test]
fn arm_labels_are_distinct_and_say_the_tool_count() {
    // The labels end up in the report; two arms that read the same are a
    // scorecard nobody can act on.
    assert_ne!(Arm::Control.label(), Arm::Gateway.label());
    assert!(Arm::Control.label().contains('6'));
    assert!(Arm::Gateway.label().contains('5'));
}

// ---------------------------------------------------------------------------
// The prompt: it must name tools the arm actually has.
//
// This is the bug that invalidated three A/B runs. The gateway arm received
// TASK_PREFIX_SHELL ("1) run_command to investigate") because `allow_shell` is
// a per-task permission flag and the selector trusted it over the registry —
// so the model was told every turn to lead with a tool that was not on its
// menu. It spent ~2 extra turns per task hunting for it, a uniform ~5,000-token
// overhead on tasks whose whole fixture is 1.4 KB.
//
// The result read as "the gateway costs 226% of the control's context". It was
// measuring a prompt mismatch.
// ---------------------------------------------------------------------------

/// Drive one agent turn and capture the system prompt it was given.
fn system_prompt_for(registry: &sc_tools::ToolRegistry) -> String {
    use std::sync::{Arc, Mutex};

    let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink_seen = seen.clone();
    let sink = sc_core::FnSink(move |ev: &sc_core::AgentEvent| {
        if let sc_core::AgentEvent::PromptAssembled { messages, .. } = ev {
            if let Some(m) = messages.iter().find(|m| m.role == "system") {
                *sink_seen.lock().unwrap() = m.content.clone();
            }
        }
    });

    let dir = fixture();
    // One canned reply, then the run ends: enough to assemble one prompt.
    let backend = sc_model::MockBackend::new([r#"{"tool":"finish"}"#]);
    let mut cfg = sc_core::AgentConfig {
        verbose: true,
        ..Default::default()
    };
    cfg.permission.allow_shell = true; // the flag that misled the selector
    let strategy = sc_core::select_strategy(&backend.capabilities());
    let _ = sc_core::run_agent_observed(
        &backend,
        None,
        registry,
        strategy.as_ref(),
        "make the test pass",
        dir.path(),
        &cfg,
        &sink,
    );
    let out = seen.lock().unwrap().clone();
    out
}

#[test]
fn the_prompt_never_names_a_tool_the_arm_does_not_have() {
    for (label, registry) in [
        ("control", control_registry()),
        ("gateway", gateway_registry()),
    ] {
        let prompt = system_prompt_for(&registry);
        assert!(!prompt.is_empty(), "{label}: no system prompt captured");

        // Only the LOOP INSTRUCTIONS, not the tool menu the strategy appends.
        // The menu lists the registry verbatim and is correct by construction;
        // the instructions are the part that was naming absent tools.
        let instructions = prompt
            .split("Each turn, respond with")
            .next()
            .unwrap_or(&prompt)
            .to_string();
        let prompt = instructions;

        let have: Vec<&str> = registry.specs().iter().map(|s| s.name).collect();
        // Every tool the registry does NOT have must be absent from the loop
        // instructions. `run_command` is excepted where the prompt names it to
        // say it is BLOCKED, which is guidance, not an instruction to call it.
        for absent in ["read_file", "edit_file", "write_file", "ask"] {
            if have.contains(&absent) {
                continue;
            }
            assert!(
                !prompt.contains(absent),
                "{label}: prompt names {absent}, which this registry does not offer:
{prompt}"
            );
        }
    }
}

#[test]
fn the_gateway_prompt_names_ask() {
    // The positive half: it is not enough to omit the wrong tool, the model has
    // to be told what it DOES have.
    let prompt = system_prompt_for(&gateway_registry());
    assert!(
        prompt.contains("ask"),
        "the gateway prompt never mentions its own reading tool:
{prompt}"
    );
}

#[test]
fn a_shell_less_registry_does_not_get_the_shell_prompt() {
    // The selector used to key off `allow_shell` alone. The permission flag says
    // the policy WOULD permit a shell; only the registry says one is on the menu.
    let prompt = system_prompt_for(&gateway_registry());
    assert!(
        !prompt.contains("run_command to investigate"),
        "a registry with no shell was told to lead with run_command:
{prompt}"
    );
}
