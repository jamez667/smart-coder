//! A native tool call survives the loop and goes back out as a native tool call.
//!
//! The harness used to flatten every native call to a bare `{"tool":...}` string and
//! replay it as plain assistant content. From turn 2 on, a model whose chat template
//! wraps calls in its own markup (`<tool_call>...</tool_call><|im_end|>` for the
//! ChatML family, Mellum2 included) therefore read its OWN history in a format its
//! template never emits — so it imitated the history and never reached the stop token,
//! because the token that ends its turn lives in the wrapper it was never shown.
//!
//! Isolated on Mellum2-12B against the same server with the same six tools: 24
//! completion tokens and a clean stop given a faithful history, 3,072 (the cap) and
//! `{"tool":"finish"}</tool_call>` repeated ~60 times given the flattened one. It cost
//! 3 of the first 6 ladder rungs and 350-400k tokens per failure. Tiel tolerated it,
//! which is the only reason it stayed hidden.
//!
//! `sc-model` pins the wire shape; these tests pin the LOOP — that the call the backend
//! returned is what the loop stores and what the next request carries.

use std::path::PathBuf;
use std::sync::Mutex;

use sc_core::{run_agent_observed, AgentConfig, FnSink, NativeTools, ParseRepair};
use sc_model::{
    CallbackBackend, Capabilities, GenerateRequest, GenerateResponse, Message, Role,
    ToolCallRecord, ToolCalling,
};
use sc_tools::default_registry;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-native-rt-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A reply carrying a native tool call: the normalised text every extractor reads,
/// plus the structured call the backend actually returned.
fn native(name: &str, args: &str, id: &str) -> GenerateResponse {
    let text = {
        let mut o = serde_json::Map::new();
        o.insert("tool".into(), serde_json::Value::String(name.into()));
        if let Some(m) = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.as_object().cloned())
        {
            for (k, v) in m {
                o.insert(k, v);
            }
        }
        serde_json::Value::Object(o).to_string()
    };
    let mut r = GenerateResponse::new(text);
    r.tool_calls = vec![ToolCallRecord::new(id, name, args)];
    r
}

/// Drive the loop over `replies` and hand back every request it made.
fn requests_for(
    replies: Vec<GenerateResponse>,
    caps: Capabilities,
    strategy: &dyn sc_core::ToolCallStrategy,
) -> Vec<Vec<Message>> {
    let ws = temp("run");
    std::fs::write(ws.join("lib.rs"), "fn main() {}\n").unwrap();

    let seen: Mutex<Vec<Vec<Message>>> = Mutex::new(Vec::new());
    let queue = Mutex::new(std::collections::VecDeque::from(replies));
    let backend = CallbackBackend::new("scripted", caps, |req: &GenerateRequest| {
        seen.lock().unwrap().push(req.messages.clone());
        Ok(queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| GenerateResponse::new(r#"{"tool":"finish"}"#)))
    });

    let cfg = AgentConfig {
        max_steps: 4,
        verify_command: None,
        ..AgentConfig::default()
    };
    let sink = FnSink(|_: &sc_core::AgentEvent| {});
    let _ = run_agent_observed(
        &backend,
        None,
        &default_registry(),
        strategy,
        "read lib.rs then finish",
        &ws,
        &cfg,
        &sink,
    );
    let _ = std::fs::remove_dir_all(&ws);
    seen.into_inner().unwrap()
}

fn native_caps() -> Capabilities {
    Capabilities {
        max_context_tokens: 32_768,
        tool_calling: ToolCalling::OpenAiStyle,
        on_device: false,
    }
}

fn text_caps() -> Capabilities {
    Capabilities {
        max_context_tokens: 32_768,
        tool_calling: ToolCalling::None,
        on_device: false,
    }
}

/// **THE REGRESSION, through the loop.** Turn 1 answers with a native call; turn 2's
/// request must carry that call structurally, with its observation paired to it — not
/// a flattened string masquerading as assistant prose.
#[test]
fn a_native_call_reaches_the_next_request_as_a_native_call() {
    let reqs = requests_for(
        vec![
            native("read_file", r#"{"path":"lib.rs"}"#, "call_1"),
            native("finish", r#"{"summary":"done"}"#, "call_2"),
        ],
        native_caps(),
        &NativeTools,
    );
    assert!(reqs.len() >= 2, "the loop took a second turn");

    let second = &reqs[1];
    let action = second
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("turn 1's action is replayed");

    // The structured call is there, verbatim.
    assert_eq!(
        action.tool_calls,
        vec![ToolCallRecord::new(
            "call_1",
            "read_file",
            r#"{"path":"lib.rs"}"#
        )],
        "the call the backend returned must be replayed as a CALL"
    );
    // The normalised text is still on the message — every extractor above reads it.
    assert!(action.content.contains(r#""tool":"read_file""#));

    // And its observation is a tool RESULT naming that call, not a bare user message.
    let result = second
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("the observation is paired to the call");
    assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
    assert!(
        result.content.contains("fn main"),
        "the file the tool read: {}",
        result.content
    );
}

/// **The text path is untouched.** `ParseRepair` produces no native call, so turn 2's
/// request must look exactly as it always has: a plain assistant message and a plain
/// user observation, with no tool fields anywhere. This is what keeps Tiel — and every
/// recorded run — behaving identically.
#[test]
fn a_text_decoded_turn_replays_as_plain_text() {
    let reqs = requests_for(
        vec![
            GenerateResponse::new(r#"{"tool":"read_file","path":"lib.rs"}"#),
            GenerateResponse::new(r#"{"tool":"finish","summary":"done"}"#),
        ],
        text_caps(),
        &ParseRepair,
    );
    assert!(reqs.len() >= 2);

    let second = &reqs[1];
    assert!(
        second.iter().all(|m| m.tool_calls.is_empty()),
        "no native call anywhere on the text path"
    );
    assert!(
        second.iter().all(|m| m.tool_call_id.is_none()),
        "and no paired result ids"
    );
    assert!(
        second.iter().all(|m| m.role != Role::Tool),
        "the observation stays a plain user message"
    );
    let action = second
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("the action is replayed");
    assert_eq!(action.content, r#"{"tool":"read_file","path":"lib.rs"}"#);
}

/// The prefix-stability property still holds with native calls in the window: turn N's
/// messages are a prefix of turn N+1's. A prompt whose prefix shifts costs the server a
/// full re-prefill every turn, which on a long run dominates everything else.
#[test]
fn native_calls_keep_the_prompt_prefix_stable() {
    let reqs = requests_for(
        vec![
            native("read_file", r#"{"path":"lib.rs"}"#, "call_1"),
            native("list_dir", r#"{"path":"."}"#, "call_2"),
            native("finish", r#"{"summary":"done"}"#, "call_3"),
        ],
        native_caps(),
        &NativeTools,
    );
    assert!(reqs.len() >= 3, "three turns: {}", reqs.len());

    for pair in reqs.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        assert!(
            next.len() > prev.len(),
            "each turn appends: {} -> {}",
            prev.len(),
            next.len()
        );
        for (i, (a, b)) in prev.iter().zip(next.iter()).enumerate() {
            assert_eq!(a.role, b.role, "message {i} changed role between turns");
            assert_eq!(a.content, b.content, "message {i} was rewritten");
            assert_eq!(
                a.tool_calls, b.tool_calls,
                "message {i}'s call was rewritten"
            );
            assert_eq!(
                a.tool_call_id, b.tool_call_id,
                "message {i}'s pairing moved"
            );
        }
    }
}

/// A truncated `write_file` reply as the token cap actually produces it: the harness's
/// normalised text AND the server's `tool_calls`, both carrying arguments that were cut
/// off mid-`content` and are therefore not valid JSON.
fn truncated_write() -> GenerateResponse {
    const ARGS: &str =
        "{\"path\":\"astar.rs\",\"content\":\"    None\n}\n\n/// A* over a caller-supplied bool grid";
    // The extractor sees the same truncated text the backend normalised from the call.
    let mut r = GenerateResponse::new(format!("{{\"tool\":\"write_file\",{}", &ARGS[1..]));
    r.tool_calls = vec![ToolCallRecord::new("call_cut", "write_file", ARGS)];
    r
}

/// **THE REGRESSION, through the loop.** A turn whose native `arguments` were truncated
/// at the token cap must not put those bytes back on the wire.
///
/// The harness salvages real work out of such a reply (`repair_truncated_file_write`), so
/// the run should continue — but replaying the malformed `arguments` inside a `tool_calls`
/// array made llama.cpp fail to parse its OWN request and answer HTTP 500, aborting the
/// task at 0 steps. Observed on `engine-diagonal-path`. The turn must degrade to the
/// pre-native shape instead: plain assistant content, plain user observation.
#[test]
fn a_truncated_native_call_is_not_replayed_as_a_native_call() {
    let reqs = requests_for(
        vec![
            truncated_write(),
            native("finish", r#"{"summary":"done"}"#, "call_2"),
        ],
        native_caps(),
        &NativeTools,
    );
    assert!(
        reqs.len() >= 2,
        "the loop survived the truncation and went on"
    );

    let second = &reqs[1];
    // Nothing on this request carries unparseable arguments...
    for m in second {
        for tc in &m.tool_calls {
            assert!(
                serde_json::from_str::<serde_json::Value>(&tc.arguments).is_ok(),
                "a call with invalid JSON arguments reached the request: {:?}",
                tc.arguments
            );
        }
    }
    // ...and specifically, the truncated turn kept none.
    let action = second
        .iter()
        .find(|m| m.role == Role::Assistant && m.content.contains("write_file"))
        .expect("turn 1's action is replayed");
    assert!(
        action.tool_calls.is_empty(),
        "the truncated call must be dropped, not replayed: {:?}",
        action.tool_calls
    );

    // The observation stays consistent with it: a plain user message, no dangling id.
    assert!(
        second
            .iter()
            .all(|m| m.tool_call_id.as_deref() != Some("call_cut")),
        "no result may name a call that is no longer in the history"
    );
    let shipped: std::collections::HashSet<String> = second
        .iter()
        .flat_map(|m| m.tool_calls.iter().map(|tc| tc.wire_id()))
        .collect();
    for m in second {
        if let Some(id) = &m.tool_call_id {
            assert!(shipped.contains(id), "dangling tool_call_id {id}");
        }
    }
}
