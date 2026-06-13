//! JNI bridge between the Kotlin Android app and the portable Rust agent core.
//!
//! Direction of calls (spec 12):
//! - **Kotlin → Rust:** [`Java_dev_dumbcoder_android_NativeBridge_runTask`] hands a
//!   task + workspace path to the agent loop.
//! - **Rust → Kotlin:** for each model turn the loop calls *up* into Kotlin's
//!   `onGenerate(prompt, maxTokens, temperature): String`, which runs AICore
//!   (Gemma 4 / Gemini Nano 4) and returns the generated text.
//!
//! ## Status
//! This **compiles on the host** (CI checks it against the real `jni` API), but
//! its on-device behaviour is **untested here** — there's no JVM/Android/device in
//! the build environment. The Kotlin method names/signatures in
//! `../../android/kotlin/NativeBridge.kt` must match the constants below. The pure
//! helper [`flatten_messages`] is unit-tested.

use std::cell::RefCell;
use std::path::Path;

use dc_core::{run_agent, AgentConfig};
use dc_model::{CallbackBackend, GenerateRequest, GenerateResponse, Message, Role};
use dc_proto::{DcError, Result};

// Must match the Kotlin side (android/kotlin/NativeBridge.kt).
const CALLBACK_METHOD: &str = "onGenerate";
// (String prompt, int maxTokens, float temperature) -> String
const CALLBACK_SIG: &str = "(Ljava/lang/String;IF)Ljava/lang/String;";

/// Flatten a chat transcript into a single prompt string for the AICore Prompt
/// API (which takes one text prompt). Pure and host-testable.
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

/// Kotlin → Rust. Run the agent on `task` within `workspace`, driving inference
/// back up into Kotlin/AICore. Returns a short summary string.
///
/// # Safety
/// Standard JNI contract: called by the JVM with valid env/args.
#[no_mangle]
pub extern "system" fn Java_dev_dumbcoder_android_NativeBridge_runTask<'local>(
    mut env: JNIEnv<'local>,
    this: JObject<'local>,
    task: JString<'local>,
    workspace: JString<'local>,
) -> jstring {
    // Marshal inputs up front while we still have direct access to `env`.
    let task: String = match env.get_string(&task) {
        Ok(s) => s.into(),
        Err(e) => return error_string(&mut env, &format!("bad task arg: {e}")),
    };
    let workspace: String = match env.get_string(&workspace) {
        Ok(s) => s.into(),
        Err(e) => return error_string(&mut env, &format!("bad workspace arg: {e}")),
    };

    // Run the agent in an inner scope so the backend (which borrows `env`) is
    // dropped before we use `env` again to build the return value.
    let outcome: Result<String> = {
        let env_cell = RefCell::new(&mut env);

        let backend = CallbackBackend::android_core(|req: &GenerateRequest| {
            call_kotlin_generate(&env_cell, &this, req)
        });

        run_agent(
            &backend,
            &task,
            Path::new(&workspace),
            &AgentConfig::default(),
        )
        .map(|report| format!("finished={} steps={}", report.finished, report.steps))
    };

    match outcome {
        Ok(summary) => new_string_or_null(&mut env, &summary),
        Err(e) => error_string(&mut env, &e.to_string()),
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
