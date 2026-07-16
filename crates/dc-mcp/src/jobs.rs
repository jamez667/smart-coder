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
    /// One or more OpenAI-compatible backend URLs. Each new job is assigned one by
    /// round-robin, so several backend pools (e.g. one llama.cpp server per GPU)
    /// are used evenly without an external load balancer. Always has ≥1 entry.
    pub base_urls: Vec<String>,
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
    /// Which backend URL this job was dispatched to (for the status snapshot).
    backend: String,
    /// The last N raw NDJSON event lines, for a compact status tail.
    recent: Vec<String>,
    /// The `Stopped { reason }` payload once the run ends (e.g. "Finished").
    stop_reason: Option<String>,
    /// The child's exit code once it exits.
    exit_code: Option<i32>,
    /// Anything the launch/read path went wrong with.
    error: Option<String>,
    /// Whether the agent successfully edited the workspace at least once (a successful
    /// `edit_file`/`write_file`). Used to tell "made changes but couldn't verify" apart from
    /// "did nothing".
    edited: bool,
    /// Whether a verification run ever went green. When false but `edited` is true, the changes
    /// are unverified (the caller should check the diff), not necessarily wrong.
    verified_green: bool,
}

impl Job {
    fn new(backend: String) -> Self {
        Self {
            state: State::Running,
            backend,
            recent: Vec::new(),
            stop_reason: None,
            exit_code: None,
            error: None,
            edited: false,
            verified_green: false,
        }
    }

    /// A plain-language outcome once the job has finished (`None` while running). Turns the
    /// (state, edited, verified) triple into something a caller can trust rather than guessing
    /// from a bare exit code — the whole point is that correct-but-unverified work reads as
    /// "check the diff", not "failed".
    fn outcome(&self) -> Option<String> {
        if self.state == State::Running {
            return None;
        }
        if self.error.is_some() {
            return Some("failed to launch".to_string());
        }
        Some(
            match (self.edited, self.verified_green) {
                // Verification runs on the HOST (the caller), not in the container, so the normal
                // success case is "edited" — the caller then runs cargo/pytest/the Unity Editor.
                (true, true) => "verified — edits made and in-container verification passed",
                (true, false) => "edited — changes were made; verify on the host (cargo / pytest \
                     / Unity Editor) and re-delegate with the failure if it's wrong",
                (false, _) => "no changes made",
            }
            .to_string(),
        )
    }
}

/// A snapshot of a job for the `status` tool (decoupled from the locked record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStatus {
    pub state: State,
    pub backend: String,
    pub stop_reason: Option<String>,
    pub recent_events: Vec<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// A plain-language outcome for the caller, so correct-but-unverified work isn't read as
    /// failure. `None` while still running; once finished, one of: "verified" (edits + a green
    /// verify), "edited, unverified" (edits made but verification never went green — check the
    /// diff), "no changes made", or "failed to launch".
    pub outcome: Option<String>,
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
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("j{seq}");
        // Round-robin across the configured backends: job N goes to pool N % len.
        // (seq is 1-based, so the first job lands on index 0.)
        let base_url = &self.cfg.base_urls[(seq as usize - 1) % self.cfg.base_urls.len()];

        let mut cmd = Command::new(&self.cfg.binary);
        cmd.arg(mode.subcommand())
            .arg(task)
            .arg("--json")
            .arg("--base-url")
            .arg(base_url)
            .arg("--model")
            .arg(&self.cfg.model)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // NB: no `--verify` is passed. Verification of a delegated task happens on the HOST (the
        // caller runs cargo/pytest/the Unity Editor and re-delegates on failure), not in this
        // slim container — see the Dockerfile. With no verify command the agent's finish gate
        // allows a clean stop after editing (dc_core::gate_finish → Allow), so it edits and
        // finishes instead of looping against a toolchain it doesn't have. The `outcome` field
        // then reports "edited, unverified — review the diff" for the caller to verify.
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

        let job = Arc::new(Mutex::new(Job::new(base_url.clone())));
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
            backend: j.backend.clone(),
            stop_reason: j.stop_reason.clone(),
            recent_events: j.recent.clone(),
            exit_code: j.exit_code,
            error: j.error.clone(),
            outcome: j.outcome(),
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
    match val.get("type").and_then(|t| t.as_str()) {
        Some("Stopped") => job.stop_reason = Some(stringify_reason(&val["reason"])),
        // A successful edit/write tool result means the workspace changed at least once.
        Some("ToolResult") => {
            let ok = val.get("is_error").and_then(|e| e.as_bool()) != Some(true);
            let summary = val.get("summary").and_then(|s| s.as_str()).unwrap_or("");
            if ok && (summary.starts_with("edit_file") || summary.starts_with("write_file")) {
                job.edited = true;
            }
        }
        // A green verification is the real success signal.
        Some("Verification") => {
            if val.get("green").and_then(|g| g.as_bool()) == Some(true) {
                job.verified_green = true;
            }
        }
        _ => {}
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
        Job::new("test-backend".to_string())
    }

    #[test]
    fn round_robin_spreads_jobs_across_backends() {
        // The dispatch index is (seq-1) % len, seq being the 1-based job counter.
        // With two backends, jobs alternate; with one, all land on it.
        let two = ["a", "b"];
        let picks: Vec<&str> = (1..=5).map(|seq| two[(seq - 1) % two.len()]).collect();
        assert_eq!(picks, ["a", "b", "a", "b", "a"]);

        let one = ["only"];
        let picks: Vec<&str> = (1..=3).map(|seq| one[(seq - 1) % one.len()]).collect();
        assert_eq!(picks, ["only", "only", "only"]);
    }

    #[test]
    fn ingests_finished_stop_reason() {
        let mut j = blank();
        ingest_event(r#"{"type":"Stopped","reason":"Finished"}"#, &mut j);
        assert_eq!(j.stop_reason.as_deref(), Some("Finished"));
    }

    #[test]
    fn tracks_edits_and_green_verification_for_the_outcome() {
        let mut j = blank();
        assert_eq!(j.outcome(), None, "no outcome while running");
        // A successful edit flips `edited`; a failed one does not.
        ingest_event(
            r#"{"type":"ToolResult","summary":"edit_file math_utils.py ok","is_error":false}"#,
            &mut j,
        );
        assert!(j.edited);
        // Edited but not verified in-container → "edited" (host verifies), NOT a failure.
        ingest_event(r#"{"type":"Stopped","reason":"Finished"}"#, &mut j);
        j.state = State::Done;
        let o = j.outcome().unwrap();
        assert!(o.starts_with("edited") && o.contains("host"), "got: {o}");
        // A green verification upgrades it to "verified".
        ingest_event(r#"{"type":"Verification","green":true}"#, &mut j);
        assert!(j.verified_green);
        assert!(j.outcome().unwrap().starts_with("verified"));
    }

    #[test]
    fn outcome_reports_no_changes_when_nothing_edited() {
        let mut j = blank();
        j.state = State::Done;
        assert_eq!(j.outcome().as_deref(), Some("no changes made"));
    }


    #[test]
    fn ingests_detailed_stop_reason() {
        let mut j = blank();
        ingest_event(
            r#"{"type":"Stopped","reason":{"Stalled":"looping on edit"}}"#,
            &mut j,
        );
        assert_eq!(j.stop_reason.as_deref(), Some("Stalled: looping on edit"));
    }

    #[test]
    fn ignores_unrelated_and_malformed_lines() {
        let mut j = blank();
        ingest_event(
            r#"{"type":"ToolCall","tool":"read_file","arg":"x"}"#,
            &mut j,
        );
        ingest_event("not json at all", &mut j);
        assert!(j.stop_reason.is_none());
    }

    #[test]
    fn mode_maps_to_subcommand() {
        assert_eq!(Mode::Run.subcommand(), "run");
        assert_eq!(Mode::Swarm.subcommand(), "swarm");
    }
}
