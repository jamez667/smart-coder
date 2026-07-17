//! The tool surface Claude Code sees: the JSON-Schema manifest for `tools/list`
//! and the dispatch that backs `tools/call`.
//!
//! Three tools, matching the fire-and-poll design:
//!   * `dumb_coder_code`   — start a coding job, return its id immediately.
//!   * `dumb_coder_status` — poll a job id for a compact status snapshot.
//!   * `dumb_coder_health` — check the local model backend is reachable.
//!
//! There are no tests here; "done" is the model's own `finish` (dumb-coder's TDD
//! suite gate is off — Claude verifies the diff afterward).

use serde_json::{json, Value};

use crate::jobs::{JobStore, State};

/// The set of tools the server exposes. A trait so [`crate::protocol`] can be
/// unit-tested against a stub without spawning real subprocesses.
pub trait Tools {
    /// Run tool `name` with `args`. `Ok(text)` is a normal result; `Err(text)` is
    /// a tool-level failure (surfaced to Claude as `isError`, not a crash).
    fn call(&self, name: &str, args: &Value) -> Result<String, String>;
}

/// The `tools/list` payload: one JSON-Schema-described tool per entry.
pub fn tool_manifest() -> Value {
    json!([
        {
            "name": "dumb_coder_code",
            "description":
                "Delegate a coding task to a local small-model agent (dumb-coder) that \
                 edits files in the target workspace directly. The task is always run \
                 through the STAGED decomposition engine: dumb-coder plans the change, \
                 breaks it into scoped stages, and lands each one gated by a per-stage \
                 build check — so multi-file changes land coherently rather than stalling. \
                 Returns a job id immediately (fire-and-poll) — call dumb_coder_status \
                 with the id to check progress. Run several in parallel by issuing \
                 multiple calls at once. Verify the diff yourself afterward (e.g. git \
                 diff + your own tests). Note: planning runs a few model phases before the \
                 first edit, so even small tasks take a little to start.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The coding instruction. Be concrete and self-contained."
                    },
                    "workspace": {
                        "type": "string",
                        "description": "Absolute path to the directory to work in. \
                                        Defaults to the server's working directory."
                    }
                },
                "required": ["task"]
            }
        },
        {
            "name": "dumb_coder_status",
            "description":
                "Poll a dumb_coder_code job by id. Returns its state (running/done/failed), \
                 the stop reason once finished, the tail of its event stream, and exit code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "The id returned by dumb_coder_code." }
                },
                "required": ["job_id"]
            }
        },
        {
            "name": "dumb_coder_health",
            "description":
                "Check that the local model backend dumb-coder relies on is reachable \
                 before delegating work. Returns the doctor report.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// The production [`Tools`], backed by a [`JobStore`]. `default_workspace` is used
/// when a `code` call omits `workspace`. `health` runs a synchronous `doctor`.
pub struct StoreTools {
    pub store: JobStore,
    pub default_workspace: String,
    /// The dumb-coder binary + backend, for the synchronous `health` check.
    pub binary: String,
    pub base_url: String,
    pub model: String,
}

impl Tools for StoreTools {
    fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        match name {
            "dumb_coder_code" => self.code(args),
            "dumb_coder_status" => self.status(args),
            "dumb_coder_health" => self.health(),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

impl StoreTools {
    fn code(&self, args: &Value) -> Result<String, String> {
        let task = args
            .get("task")
            .and_then(|t| t.as_str())
            .filter(|t| !t.trim().is_empty())
            .ok_or("dumb_coder_code requires a non-empty 'task'")?;
        let workspace = args
            .get("workspace")
            .and_then(|w| w.as_str())
            .unwrap_or(&self.default_workspace);

        let id = self.store.start(task, workspace)?;
        Ok(json!({
            "job_id": id,
            "state": "running",
            "mode": "staged",
            "workspace": workspace,
            "hint": "poll dumb_coder_status with this job_id; verify the diff when done",
        })
        .to_string())
    }

    fn status(&self, args: &Value) -> Result<String, String> {
        let id = args
            .get("job_id")
            .and_then(|j| j.as_str())
            .ok_or("dumb_coder_status requires a 'job_id'")?;
        let st = self
            .store
            .status(id)
            .ok_or_else(|| format!("no such job: {id}"))?;

        Ok(json!({
            "job_id": id,
            "state": st.state.as_str(),
            "backend": st.backend,
            "stop_reason": st.stop_reason,
            "finished_ok": st.state == State::Done
                && st.stop_reason.as_deref() == Some("Finished"),
            "outcome": st.outcome,
            "exit_code": st.exit_code,
            "error": st.error,
            "recent_events": st.recent_events,
        })
        .to_string())
    }

    fn health(&self) -> Result<String, String> {
        let out = std::process::Command::new(&self.binary)
            .arg("doctor")
            .arg("--base-url")
            .arg(&self.base_url)
            .arg("--model")
            .arg(&self.model)
            .output()
            .map_err(|e| format!("failed to run {} doctor: {e}", self.binary))?;
        let report = String::from_utf8_lossy(&out.stdout);
        if out.status.success() {
            Ok(report.to_string())
        } else {
            let errtail = String::from_utf8_lossy(&out.stderr);
            Err(format!("backend not healthy:\n{report}\n{errtail}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_the_three_tools() {
        let m = tool_manifest();
        let names: Vec<&str> = m
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["dumb_coder_code", "dumb_coder_status", "dumb_coder_health"]
        );
    }

    #[test]
    fn code_schema_requires_task_only() {
        let m = tool_manifest();
        let code = &m[0];
        assert_eq!(code["inputSchema"]["required"], json!(["task"]));
        // workspace is the only optional property (mode is always staged now).
        let props = &code["inputSchema"]["properties"];
        assert!(props.get("workspace").is_some());
        assert!(props.get("decompose").is_none(), "decompose toggle was removed");
    }

    #[test]
    fn status_schema_requires_job_id() {
        let m = tool_manifest();
        let status = &m[1];
        assert_eq!(status["inputSchema"]["required"], json!(["job_id"]));
    }
}
