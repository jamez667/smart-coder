//! Transcript logging — an always-on, append-only record of every model call (the full
//! prompt sent and the full reply received), so a session can be handed over verbatim for
//! debugging instead of copy-pasting the on-screen debug echo.
//!
//! Because EVERY inference in the app funnels through [`crate::OpenAiBackend`]'s
//! `generate`/`generate_streaming`, logging here captures the whole picture — chat classify +
//! generate calls, agent turns, the health probe, swarm — with no logging scattered elsewhere.
//!
//! One JSONL file per process (`transcript-<timestamp>-<pid>.jsonl`), kept across sessions, in
//! `SC_LOG_DIR` (the app sets this to `%APPDATA%\smart-coder\logs`; falls back to the system
//! temp dir). Set `SC_NO_LOG=1` to disable. Best-effort: any IO error silently disables logging
//! rather than disrupting a run.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// The process-wide transcript file, opened once on first use. `None` once logging is disabled
/// (env off, or an IO error). Wrapped in a `Mutex` because chat, agent, and probe threads all
/// log concurrently.
static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Whether transcript logging is active this process (false when `SC_NO_LOG` is set or the log
/// file could not be opened).
pub fn is_enabled() -> bool {
    log_file().is_some()
}

/// The path of this process's transcript file, if logging is on. Handy for surfacing "your log
/// is here" in the UI.
pub fn path() -> Option<PathBuf> {
    // Recompute deterministically the same way `open` did — cached separately so callers can
    // show it without holding the file lock.
    LOG_PATH.get().cloned().flatten()
}

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn log_file() -> Option<&'static Mutex<File>> {
    LOG.get_or_init(open).as_ref()
}

/// Open (create) the per-process transcript file, honoring `SC_NO_LOG` and `SC_LOG_DIR`.
fn open() -> Option<Mutex<File>> {
    if std::env::var_os("SC_NO_LOG").is_some() {
        let _ = LOG_PATH.set(None);
        return None;
    }
    let dir = std::env::var_os("SC_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("smart-coder").join("logs"));
    if std::fs::create_dir_all(&dir).is_err() {
        let _ = LOG_PATH.set(None);
        return None;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("transcript-{stamp}-{}.jsonl", std::process::id()));
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            let _ = LOG_PATH.set(Some(path));
            Some(Mutex::new(f))
        }
        Err(_) => {
            let _ = LOG_PATH.set(None);
            None
        }
    }
}

/// A single logged model call: the request that went out and what came back.
pub struct Entry<'a> {
    /// Which backend method ran (`"generate"` / `"generate_streaming"`).
    pub call: &'a str,
    /// The model name the request targeted.
    pub model: &'a str,
    /// The endpoint URL.
    pub endpoint: &'a str,
    /// The full ordered messages: `(role, content)` verbatim, exactly as sent.
    pub messages: &'a [(&'a str, &'a str)],
    /// The output constraint kind + its payload (e.g. the GBNF grammar), or `None`.
    pub constraint: Option<(&'a str, &'a str)>,
    pub temperature: f32,
    pub max_tokens: u32,
    /// The full reply text, or the error string if the call failed.
    pub result: Result<&'a str, &'a str>,
    /// Wall-clock duration of the call, in milliseconds.
    pub ms: u128,
}

/// Append one call to the transcript (best-effort — a disabled log or IO error is a no-op).
pub fn log(entry: Entry) {
    let Some(lock) = log_file() else { return };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = record(&entry, stamp).to_string();
    // One JSON object per line. Hold the lock only for the write.
    if let Ok(mut f) = lock.lock() {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

/// Build the JSON record for one entry (pure — no IO), so the serialized shape is testable.
fn record(entry: &Entry, ts_ms: u128) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = entry
        .messages
        .iter()
        .map(|(role, content)| serde_json::json!({ "role": role, "content": content }))
        .collect();
    let (ok, reply, error) = match entry.result {
        Ok(text) => (true, Some(text), None),
        Err(e) => (false, None, Some(e)),
    };
    serde_json::json!({
        "ts_ms": ts_ms,
        "call": entry.call,
        "model": entry.model,
        "endpoint": entry.endpoint,
        "temperature": entry.temperature,
        "max_tokens": entry.max_tokens,
        "constraint": entry.constraint.map(|(kind, payload)| {
            serde_json::json!({ "kind": kind, "payload": payload })
        }),
        "messages": messages,
        "ok": ok,
        "reply": reply,
        "error": error,
        "ms": entry.ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_captures_full_prompt_and_reply() {
        let entry = Entry {
            call: "generate",
            model: "qwen3-coder-30b",
            endpoint: "http://localhost:11435/v1/chat/completions",
            messages: &[("system", "You are helpful."), ("user", "hello")],
            constraint: Some(("grammar", "root ::= \"chat\"")),
            temperature: 0.4,
            max_tokens: 1200,
            result: Ok("Hi there!"),
            ms: 42,
        };
        let v = record(&entry, 1234);
        assert_eq!(v["call"], "generate");
        assert_eq!(v["model"], "qwen3-coder-30b");
        assert_eq!(v["ok"], true);
        assert_eq!(v["reply"], "Hi there!");
        assert!(v["error"].is_null());
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][1]["content"], "hello");
        assert_eq!(v["constraint"]["kind"], "grammar");
        assert_eq!(v["ms"], 42);
    }

    #[test]
    fn record_captures_errors() {
        let entry = Entry {
            call: "generate_streaming",
            model: "m",
            endpoint: "e",
            messages: &[("user", "hi")],
            constraint: None,
            temperature: 0.0,
            max_tokens: 8,
            result: Err("connection refused"),
            ms: 5,
        };
        let v = record(&entry, 1);
        assert_eq!(v["ok"], false);
        assert!(v["reply"].is_null());
        assert_eq!(v["error"], "connection refused");
        assert!(v["constraint"].is_null());
    }
}
