//! The job store: fire-and-poll over `dumb-coder run/swarm --json` subprocesses.
//!
//! A [`JobStore`] spawns the `dumb-coder` binary headless, reading its NDJSON
//! event stream on a background thread into a shared [`Job`] record. The MCP
//! `code` tool starts a job and returns its id immediately; the `status` tool
//! reads the record. No async runtime — one blocking reader thread per job,
//! mirroring `dc-web`'s worker-thread model over the same event stream.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How the child was launched — a single focused agent loop, or dumb-coder's
/// own orchestrator+workers decomposition (Claude picks per task size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `dumb-coder run --json` — one agent loop, fast, for focused tasks.
    Run,
    /// `dumb-coder swarm --json` — decompose across parallel workers.
    Swarm,
}

impl Mode {
    fn subcommand(self) -> &'static str {
        match self {
            Mode::Run => "run",
            Mode::Swarm => "swarm",
        }
    }
}

/// The lifecycle of a job, as reported to Claude by the `status` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Done,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
        }
    }
}

/// Backend + launch configuration shared by every job the store spawns.
#[derive(Debug, Clone)]
pub struct JobConfig {
    /// Path to the `dumb-coder` binary to invoke.
    pub binary: String,
    /// OpenAI-compatible backend URL passed as `--base-url`.
    pub base_url: String,
    /// Model tag passed as `--model`.
    pub model: String,
    /// Pre-approve `run_command` shell calls (`--yolo`) — a headless run can't
    /// prompt, so without this the model stalls the moment it needs a command.
    pub yolo: bool,
}

/// One tracked job: the child handle plus the mutable record the reader thread
/// updates and the `status` tool reads.
#[derive(Debug)]
struct Job {
    state: State,
    /// The last N raw NDJSON event lines, for a compact status tail.
    recent: Vec<String>,
    /// The `Stopped { reason }` payload once the run ends (e.g. "Finished").
    stop_reason: Option<String>,
    /// The child's exit code once it exits.
    exit_code: Option<i32>,
    /// Anything the launch/read path went wrong with.
    error: Option<String>,
}

impl Job {
    fn new() -> Self {
        Self {
            state: State::Running,
            recent: Vec::new(),
            stop_reason: None,
            exit_code: None,
            error: None,
        }
    }
}

/// A snapshot of a job for the `status` tool (decoupled from the locked record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStatus {
    pub state: State,
    pub stop_reason: Option<String>,
    pub recent_events: Vec<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Keep the status tail bounded so polling never floods Claude's context.
const MAX_RECENT: usize = 12;

/// The shared, thread-safe job registry.
pub struct JobStore {
    cfg: JobConfig,
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Arc<Mutex<Job>>>>,
}

impl JobStore {
    pub fn new(cfg: JobConfig) -> Self {
        Self {
            cfg,
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a `dumb-coder` run for `task` in `workspace` and return its job id
    /// immediately. A background thread drains the child's NDJSON stream into the
    /// job record; the caller polls with [`JobStore::status`].
    pub fn start(&self, task: &str, workspace: &str, mode: Mode) -> Result<String, String> {
        let id = format!("j{}", self.next_id.fetch_add(1, Ordering::Relaxed));

        let mut cmd = Command::new(&self.cfg.binary);
        cmd.arg(mode.subcommand())
            .arg(task)
            .arg("--json")
            .arg("--base-url")
            .arg(&self.cfg.base_url)
            .arg("--model")
            .arg(&self.cfg.model)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // `swarm --json` needs the terminal path; `run --json` is already headless.
        if self.cfg.yolo {
            cmd.arg("--yolo");
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", self.cfg.binary))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "child produced no stdout pipe".to_string())?;

        let job = Arc::new(Mutex::new(Job::new()));
        self.jobs
            .lock()
            .expect("jobs lock")
            .insert(id.clone(), Arc::clone(&job));

        // The reader thread owns the child and the pipe for the job's lifetime.
        std::thread::spawn(move || read_events(child, stdout, job));

        Ok(id)
    }

    /// A snapshot of a job's current state, or `None` for an unknown id.
    pub fn status(&self, id: &str) -> Option<JobStatus> {
        let job = self.jobs.lock().expect("jobs lock").get(id).cloned()?;
        let j = job.lock().expect("job lock");
        Some(JobStatus {
            state: j.state,
            stop_reason: j.stop_reason.clone(),
            recent_events: j.recent.clone(),
            exit_code: j.exit_code,
            error: j.error.clone(),
        })
    }
}

/// Drain the child's NDJSON stdout into `job`, then reap the child and record its
/// exit status. Runs on a dedicated thread for the job's whole life.
fn read_events(mut child: Child, stdout: std::process::ChildStdout, job: Arc<Mutex<Job>>) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let mut j = job.lock().expect("job lock");
                j.error = Some(format!("read error: {e}"));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let mut j = job.lock().expect("job lock");
        ingest_event(&line, &mut j);
        j.recent.push(line);
        let overflow = j.recent.len().saturating_sub(MAX_RECENT);
        if overflow > 0 {
            j.recent.drain(0..overflow);
        }
    }

    // Stream closed — the child is finishing. Reap it for the exit code.
    let status = child.wait();
    let mut j = job.lock().expect("job lock");
    match status {
        Ok(s) => {
            j.exit_code = s.code();
            // "done" means the process ended cleanly; the *outcome* (finished vs
            // stalled) is in stop_reason. A non-zero exit with no stop_reason is a
            // genuine launch/crash failure.
            j.state = if j.error.is_some() || (s.code().is_none()) {
                State::Failed
            } else if j.stop_reason.is_none() && s.code() != Some(0) {
                State::Failed
            } else {
                State::Done
            };
        }
        Err(e) => {
            j.error = Some(format!("wait failed: {e}"));
            j.state = State::Failed;
        }
    }
}

/// Pull the field we surface (the stop reason) out of one NDJSON event line.
/// `AgentEvent` is internally tagged (`#[serde(tag = "type")]`), so the end event
/// is `{"type":"Stopped","reason":...}`. Unknown/irrelevant events are ignored —
/// this is best-effort enrichment on top of the raw tail, not a full
/// deserialization of the stream.
fn ingest_event(line: &str, job: &mut Job) {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if val.get("type").and_then(|t| t.as_str()) == Some("Stopped") {
        job.stop_reason = Some(stringify_reason(&val["reason"]));
    }
}

/// Render a `StopReason` JSON value as a short human string. The enum is either a
/// bare string (`"Finished"`) or a single-key object (`{"Stalled": "…"}`).
fn stringify_reason(reason: &serde_json::Value) -> String {
    if let Some(s) = reason.as_str() {
        return s.to_string();
    }
    if let Some(obj) = reason.as_object() {
        if let Some((k, v)) = obj.iter().next() {
            return match v.as_str() {
                Some(detail) => format!("{k}: {detail}"),
                None => k.clone(),
            };
        }
    }
    reason.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Job {
        Job::new()
    }

    #[test]
    fn ingests_finished_stop_reason() {
        let mut j = blank();
        ingest_event(r#"{"type":"Stopped","reason":"Finished"}"#, &mut j);
        assert_eq!(j.stop_reason.as_deref(), Some("Finished"));
    }

    #[test]
    fn ingests_detailed_stop_reason() {
        let mut j = blank();
        ingest_event(r#"{"type":"Stopped","reason":{"Stalled":"looping on edit"}}"#, &mut j);
        assert_eq!(j.stop_reason.as_deref(), Some("Stalled: looping on edit"));
    }

    #[test]
    fn ignores_unrelated_and_malformed_lines() {
        let mut j = blank();
        ingest_event(r#"{"type":"ToolCall","tool":"read_file","arg":"x"}"#, &mut j);
        ingest_event("not json at all", &mut j);
        assert!(j.stop_reason.is_none());
    }

    #[test]
    fn mode_maps_to_subcommand() {
        assert_eq!(Mode::Run.subcommand(), "run");
        assert_eq!(Mode::Swarm.subcommand(), "swarm");
    }
}
