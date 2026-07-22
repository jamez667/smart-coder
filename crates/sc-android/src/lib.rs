//! JNI bridge between the Kotlin Android app and the portable Rust agent core
//! (on-device mode). The phone runs the whole agent loop locally; inference is done
//! by Android AICore (Gemini Nano) via an up-call into Kotlin.
//!
//! Direction of calls:
//! - **Kotlin → Rust:** [`Java_com_smartcoder_remote_NativeBridge_runTask`] hands a
//!   task + workspace path to the agent loop.
//! - **Rust → Kotlin:** for each model turn the loop calls *up* into Kotlin's
//!   `onGenerate(prompt, maxTokens, temperature): String`, which runs AICore and
//!   returns the generated text.
//!
//! ## Status
//! This compiles on the host against the real `jni` API, but its on-device behavior
//! can only be verified on a device with AICore (there is no such runtime on the dev
//! box). The Kotlin method name/signature in `NativeBridge.kt` must match the
//! constants below. The pure helper [`flatten_messages`] is unit-tested.

use std::cell::RefCell;
use std::path::Path;

use std::sync::Mutex;

use sc_core::{run_agent_observed, AgentConfig, AgentEvent, FnSink};
use sc_model::{
    CallbackBackend, Capabilities, GenerateRequest, GenerateResponse, Message, ModelBackend, Role,
    ToolCalling,
};
use sc_proto::{DcError, Result};
use sc_tools::default_registry;

// Must match the Kotlin side (NativeBridge.kt).
const CALLBACK_METHOD: &str = "onGenerate";
// (String prompt, int maxTokens, float temperature) -> String
const CALLBACK_SIG: &str = "(Ljava/lang/String;IF)Ljava/lang/String;";

/// Capabilities of the on-device AICore model. A small context window and no
/// structural tool-call enforcement (Gemini Nano is a plain text completion model),
/// so the harness uses its most defensive prompt+parse+repair strategy.
fn android_caps() -> Capabilities {
    Capabilities {
        max_context_tokens: 4_096,
        tool_calling: ToolCalling::None,
        on_device: true,
    }
}

/// An `AgentConfig` tuned for Gemini Nano's constraints (tiny 4K context, 256-token
/// replies, no tool-call enforcement). The stock defaults assume a mid-size model:
/// they let the loop grind 25 steps and keep only 3 recent turns, which on Nano means
/// it forgets what it already read and re-reads the same files in a loop (observed
/// live). This trims the budget to what Nano can actually use and fails faster.
fn nano_config() -> AgentConfig {
    AgentConfig {
        // Keep MORE recent turns verbatim so Nano can see it already read a file (its tiny
        // context can't hold a rolling summary usefully — verbatim recent turns are the
        // only reliable memory). The read-loop is the #1 failure mode.
        keep_recent_turns: 6,
        // Use nearly the whole nominal window — Nano's 4K is already small; don't shave 25%.
        effective_context_fraction: 0.9,
        // Fail fast: 25 steps of re-reads is just a long way to lose. If Nano can't make
        // progress in ~12 turns it won't in 25.
        max_steps: 12,
        // Break an idempotent-repeat sooner (a tiny model loops harder).
        repeat_limit: 2,
        no_progress_limit: 2,
        ..Default::default()
    }
}

/// Flatten a chat transcript into a single prompt string for the AICore Prompt API
/// (which takes one text prompt). Pure and host-testable.
pub fn flatten_messages(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let tag = match m.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        out.push_str(tag);
        out.push_str(": ");
        out.push_str(&m.content);
        out.push_str("\n\n");
    }
    out.push_str("Assistant: ");
    out
}

// ----- JNI entry point ------------------------------------------------------

use jni::objects::{JObject, JString, JValue};
use jni::sys::jstring;
use jni::JNIEnv;

/// Kotlin → Rust. Run the agent on `task` within `workspace`, driving inference back
/// up into Kotlin/AICore. Returns a short summary string.
///
/// # Safety
/// Standard JNI contract: called by the JVM with valid env/args.
#[no_mangle]
pub extern "system" fn Java_com_smartcoder_remote_NativeBridge_runTask<'local>(
    mut env: JNIEnv<'local>,
    this: JObject<'local>,
    task: JString<'local>,
    workspace: JString<'local>,
) -> jstring {
    let task: String = match env.get_string(&task) {
        Ok(s) => s.into(),
        Err(e) => return error_string(&mut env, &format!("bad task arg: {e}")),
    };
    let workspace: String = match env.get_string(&workspace) {
        Ok(s) => s.into(),
        Err(e) => return error_string(&mut env, &format!("bad workspace arg: {e}")),
    };

    // Run in an inner scope so the backend (which borrows `env`) is dropped before we
    // reuse `env` to build the return value. A transcript sink captures each turn's
    // model output + tool result so the phone can SEE what the on-device model did
    // (otherwise a failed run is a black box — just "finished=false").
    let outcome: Result<String> = {
        let env_cell = RefCell::new(&mut env);
        let backend = CallbackBackend::new("aicore", android_caps(), |req: &GenerateRequest| {
            call_kotlin_generate(&env_cell, &this, req)
        });
        let transcript = Mutex::new(String::new());
        let sink = FnSink(|e: &AgentEvent| {
            let mut t = transcript.lock().unwrap();
            append_event(&mut t, e);
        });
        let registry = default_registry();
        let strategy = sc_core::select_strategy(&backend.capabilities());
        let result = run_agent_observed(
            &backend,
            None,
            &registry,
            strategy.as_ref(),
            &task,
            Path::new(&workspace),
            &nano_config(),
            &sink,
        );
        result.map(|report| {
            let t = transcript.into_inner().unwrap();
            format!("finished={} steps={}\n{}", report.finished, report.steps, t)
        })
    };

    match outcome {
        Ok(summary) => new_string_or_null(&mut env, &summary),
        Err(e) => error_string(&mut env, &e.to_string()),
    }
}

/// Append a one-line summary of an event to the transcript — enough to see whether the
/// model is emitting tool calls or wandering. Keeps model output short (the interesting
/// failure is malformed/prose replies where no tool call parses).
fn append_event(out: &mut String, e: &AgentEvent) {
    use std::fmt::Write;
    match e {
        AgentEvent::ModelTurn { step, raw, .. } => {
            let head: String = raw.chars().take(160).collect();
            let _ = writeln!(out, "[{step}] model> {}", head.replace('\n', " "));
        }
        AgentEvent::ToolCall { tool, arg } => {
            let _ = writeln!(out, "    call {tool} {arg}");
        }
        AgentEvent::ToolResult {
            summary, is_error, ..
        } => {
            let mark = if *is_error { "ERR" } else { "ok" };
            let _ = writeln!(out, "    {mark}: {summary}");
        }
        AgentEvent::RepairTriggered { detail } => {
            let _ = writeln!(out, "    repair: {detail}");
        }
        AgentEvent::Stalled { trigger } => {
            let _ = writeln!(out, "    stalled: {trigger}");
        }
        AgentEvent::Stopped { reason } => {
            let _ = writeln!(out, "stopped: {reason:?}");
        }
        _ => {}
    }
}

/// Rust → Kotlin up-call: invoke `onGenerate` and read back the generated text.
fn call_kotlin_generate(
    env_cell: &RefCell<&mut JNIEnv>,
    this: &JObject,
    req: &GenerateRequest,
) -> Result<GenerateResponse> {
    let prompt = flatten_messages(&req.messages);
    let mut env = env_cell.borrow_mut();

    let jprompt = env
        .new_string(&prompt)
        .map_err(|e| DcError::Backend(format!("new_string: {e}")))?;

    let ret = env.call_method(
        this,
        CALLBACK_METHOD,
        CALLBACK_SIG,
        &[
            JValue::Object(&jprompt),
            JValue::Int(req.max_tokens as i32),
            JValue::Float(req.temperature),
        ],
    );

    let ret = match ret {
        Ok(v) => v,
        Err(e) => {
            // A pending Java exception must be cleared before returning to the JVM.
            let _ = env.exception_clear();
            return Err(DcError::Backend(format!("AICore onGenerate failed: {e}")));
        }
    };

    let obj = ret
        .l()
        .map_err(|e| DcError::Backend(format!("onGenerate return not an object: {e}")))?;
    let text: String = env
        .get_string(&JString::from(obj))
        .map_err(|e| DcError::Backend(format!("reading onGenerate result: {e}")))?
        .into();

    Ok(GenerateResponse { content: text })
}

fn new_string_or_null(env: &mut JNIEnv, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}

fn error_string(env: &mut JNIEnv, msg: &str) -> jstring {
    new_string_or_null(env, &format!("error: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_includes_roles_and_trailing_assistant_cue() {
        let msgs = vec![
            Message::system("be brief"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        let s = flatten_messages(&msgs);
        assert!(s.contains("System: be brief"));
        assert!(s.contains("User: hi"));
        assert!(s.contains("Assistant: hello"));
        assert!(s.trim_end().ends_with("Assistant:"));
    }

    #[test]
    fn flatten_handles_empty() {
        assert_eq!(flatten_messages(&[]), "Assistant: ");
    }
}
