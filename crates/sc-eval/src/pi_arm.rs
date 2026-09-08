//! The pi arm: an EXTERNAL coding agent, graded by the same harness.
//!
//! [`PiSolver`] shells out to `pi` (the open-source pi coding agent) against the
//! same local model the in-tree agent uses, and hands the workspace back to the
//! unchanged `runner::run_task`. Red-first, frozen contract tests, green-after
//! and the tamper check apply exactly as they do to `AgentSolver` -- the only
//! thing that differs is who drove the edits. That makes this a calibration
//! point: if pi solves a rung this harness's agent cannot, the gap is in the
//! harness, not the model.
//!
//! # What pi is told, and what it is not
//!
//! The prompt is the task description plus the verify command, and nothing else:
//! no system-prompt tuning, no skills, no extensions, no `AGENTS.md` discovery
//! (`--no-*` for each). It gets `read,edit,write,bash` -- the closest match to
//! the measured six -- and the model catalogue in `evals/pi/agent-dir`, pointed
//! at through `PI_CODING_AGENT_DIR` so the user's own `~/.pi` is never read or
//! written.
//!
//! # `--mode json`, not text
//!
//! In text mode pi prints only the final answer; every tool call is invisible.
//! JSON mode streams one event per line, so `steps` is a real count of
//! `tool_execution_start` events and the run log is a replayable trace.
//!
//! # `--offline`, always
//!
//! Without it pi phones home at startup for a catalogue update, and on this
//! machine that stalled a 1.7-second run past a 60-second timeout with nothing
//! on either stream. The local provider needs no catalogue.
//!
//! # Windows
//!
//! `pi` is a POSIX shell script and `pi.cmd` a batch file; both do nothing but
//! `exec node <bundle>/cli.js "$@"`. The solver does that itself: CreateProcess
//! cannot run a shell script, and std refuses to hand a batch file an argument
//! containing a newline (the CVE-2024-24576 hardening) -- and the prompt has
//! newlines.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sc_core::AgentConfig;
use sc_proto::{DcError, Result};

use crate::solver::{RunInfo, Solver};
use crate::task::EvalTask;

/// The provider name in `evals/pi/agent-dir/models.json`.
pub const DEFAULT_PROVIDER: &str = "local-tiel";
/// The one model that provider lists.
pub const DEFAULT_MODEL: &str = "tiel-coder-35b";
/// pi's built-in tools closest to the measured six.
const TOOLS: &str = "read,edit,write,bash";
/// Where pi is installed on the machine this arm was built on; the last resort
/// after `PI_BIN` and the PATH.
const KNOWN_PI: &str = r"C:\Users\mail\AppData\Local\pi-node\current\pi";
/// The bundle both `pi` launchers exec, relative to the launcher's directory.
const CLI_JS: &str = "node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js";
/// The wall-clock cap when a task does not set `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Runs `pi` on a task and lets the harness grade the result.
pub struct PiSolver {
    /// The model server. pi reads the URL from `models.json`, not from here; it
    /// is recorded in the run log so a report can be matched to a server.
    url: String,
    model: String,
    provider: String,
    /// Advisory only: pi has no step cap, so the wall clock is the real limit.
    /// Recorded in the log so the two arms' budgets sit side by side.
    max_steps: usize,
    /// Fallback wall-clock cap; a task's own `timeout_secs` wins.
    timeout_secs: u64,
    pi_bin: PathBuf,
    agent_dir: PathBuf,
    out_dir: Option<PathBuf>,
    /// `pi --version`, or `unknown`.
    version: String,
    /// `pi/<version>`, the solver name.
    label: String,
    /// Per-solver run counter, for log file names.
    runs: Cell<usize>,
    last_run: RefCell<Option<RunInfo>>,
}

impl PiSolver {
    /// Build a solver against the model at `url`. `model` may be empty for the
    /// default. `cfg` supplies the step cap the in-tree agent would get, for
    /// the record.
    pub fn new(url: &str, model: &str, cfg: &AgentConfig) -> Self {
        let pi_bin = locate_pi();
        let version = pi_version(&pi_bin).unwrap_or_else(|| "unknown".to_string());
        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.to_string()
        };
        Self {
            url: url.to_string(),
            model,
            provider: DEFAULT_PROVIDER.to_string(),
            max_steps: cfg.max_steps,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            pi_bin,
            agent_dir: repo_root().join("evals").join("pi").join("agent-dir"),
            out_dir: None,
            label: format!("pi/{version}"),
            version,
            runs: Cell::new(0),
            last_run: RefCell::new(None),
        }
    }

    /// Write each run's full pi output to `<dir>/<task>-pi-<n>.log`.
    pub fn with_out_dir(mut self, dir: PathBuf) -> Self {
        self.out_dir = Some(dir);
        self
    }

    /// Use a provider other than [`DEFAULT_PROVIDER`]; it must exist in the
    /// agent dir's `models.json`.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// The `pi --version` this arm found, or `unknown`.
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn pi_bin(&self) -> &Path {
        &self.pi_bin
    }
}

/// The whole of what pi is told.
pub(crate) fn prompt_for(task: &EvalTask) -> String {
    format!(
        "{}\n\nThe tests are run with: `{}`. Make them pass. Do not modify the test files.",
        task.description, task.verify_cmd
    )
}

/// Everything after the binary. The prompt goes after `--` so a description
/// that opens with a dash is a message, not a flag.
pub(crate) fn pi_args(provider: &str, model: &str, prompt: &str) -> Vec<String> {
    [
        "--offline",
        "-p",
        "--mode",
        "json",
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--provider",
        provider,
        "--model",
        model,
        "--tools",
        TOOLS,
        "--",
        prompt,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

impl Solver for PiSolver {
    fn name(&self) -> &str {
        &self.label
    }

    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()> {
        let n = self.runs.get() + 1;
        self.runs.set(n);
        self.last_run.replace(None);

        let prompt = prompt_for(task);
        let timeout = Duration::from_secs(task.timeout_secs.unwrap_or(self.timeout_secs));

        let mut cmd = pi_command(&self.pi_bin);
        cmd.args(pi_args(&self.provider, &self.model, &prompt))
            .current_dir(workspace)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| DcError::Eval(format!("spawning pi ({}): {e}", self.pi_bin.display())))?;

        // Drain both pipes off-thread: a pi that fills one while we block on the
        // other never exits, and the timeout below would then blame the model.
        let out_handle = drain(child.stdout.take());
        let err_handle = drain(child.stderr.take());

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) => {}
                Err(_) => break None,
            }
            if Instant::now() >= deadline {
                kill_tree(&mut child);
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).into_owned();

        let steps = count_tool_calls(&stdout);
        let stop_reason = match status {
            Some(st) => match st.code() {
                Some(code) => format!("exit {code}"),
                None => "exit signal".to_string(),
            },
            None => "timeout".to_string(),
        };

        if let Some(dir) = &self.out_dir {
            let header = format!(
                "# pi arm run {n}: task {}\n\
                 # {} --provider {} --model {} (url {}; max_steps {} advisory -- pi has \
                 no step cap; timeout {}s)\n\
                 # stop: {stop_reason}; tool calls: {steps}\n",
                task.id,
                self.pi_bin.display(),
                self.provider,
                self.model,
                self.url,
                self.max_steps,
                timeout.as_secs(),
            );
            let _ = std::fs::create_dir_all(dir);
            let path = dir.join(format!("{}-pi-{n}.log", task.id));
            let body = format!("{header}--- stdout\n{stdout}\n--- stderr\n{stderr}");
            if let Err(e) = std::fs::write(&path, body) {
                eprintln!("[pi arm] could not write {}: {e}", path.display());
            }
        }

        self.last_run.replace(Some(RunInfo {
            steps,
            stop_reason: stop_reason.clone(),
            self_verified: None,
            interventions: 0,
            total_prompt_tokens: 0,
            total_cached_prompt_tokens: 0,
            total_prefilled_prompt_tokens: 0,
            peak_reply_tokens: 0,
            harness_faults: Vec::new(),
        }));

        // A pi that died before touching anything (no model server, a bad
        // provider name) is a solver error, not a STILL-RED: the workspace was
        // never attempted. One that ran and then failed is graded on what it left.
        let failed = matches!(status, Some(st) if !st.success());
        if failed && steps == 0 {
            let tail: Vec<&str> = stderr.lines().chain(stdout.lines()).rev().take(5).collect();
            return Err(DcError::Eval(format!(
                "pi {stop_reason} without a tool call: {}",
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            )));
        }
        Ok(())
    }

    fn last_run(&self) -> Option<RunInfo> {
        self.last_run.borrow().clone()
    }
}

/// One line per `tool_execution_start` event in pi's JSON stream.
fn count_tool_calls(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|l| l.starts_with(r#"{"type":"tool_execution_start""#))
        .count()
}

/// `PI_BIN`, else the first `pi` on the PATH, else the known install.
fn locate_pi() -> PathBuf {
    if let Some(p) = std::env::var_os("PI_BIN") {
        return PathBuf::from(p);
    }
    let finder = if cfg!(windows) { "where" } else { "which" };
    if let Ok(out) = Command::new(finder)
        .arg("pi")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            if let Some(line) = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
            {
                return PathBuf::from(line);
            }
        }
    }
    PathBuf::from(KNOWN_PI)
}

/// A `Command` that runs pi the way its own launchers do: node on the bundled
/// CLI when that bundle sits beside the launcher, the launcher itself otherwise.
fn pi_command(pi_bin: &Path) -> Command {
    if let Some(dir) = pi_bin.parent() {
        let cli = dir.join(CLI_JS);
        if cli.is_file() {
            let local_node = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
            let node = if local_node.is_file() {
                local_node
            } else {
                PathBuf::from("node")
            };
            let mut cmd = Command::new(node);
            cmd.arg(cli);
            return cmd;
        }
    }
    Command::new(pi_bin)
}

fn pi_version(pi_bin: &Path) -> Option<String> {
    let out = pi_command(pi_bin)
        .args(["--offline", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// Kill a timed-out pi *and everything it spawned*. Mirrors `sc-verify`'s
/// tree kill: `Child::kill` reaches only node, and the bash tool's children
/// would otherwise hold the pipes open and hang the drain threads.
fn kill_tree(child: &mut Child) {
    let pid = child.id();

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// The workspace root: `crates/sc-eval` has two ancestors. Same resolution the
/// gateway A/B binary uses, kept local so this module has no dependency on it.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/sc-eval has two ancestors")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(description: &str) -> EvalTask {
        EvalTask {
            id: "t".into(),
            description: description.into(),
            fixture: PathBuf::from("fixture"),
            verify_cmd: "cargo test -q".into(),
            contract_tests: vec!["tests/contract.rs".into()],
            solution: None,
            tags: Vec::new(),
            timeout_secs: None,
        }
    }

    #[test]
    fn the_prompt_is_the_description_and_the_verify_command_and_nothing_else() {
        let p = prompt_for(&task("Make `add` commutative."));
        assert_eq!(
            p,
            "Make `add` commutative.\n\nThe tests are run with: `cargo test -q`. \
             Make them pass. Do not modify the test files."
        );
    }

    #[test]
    fn argv_pins_the_non_interactive_isolated_offline_json_run() {
        let args = pi_args("local-tiel", "tiel-coder-35b", "do the thing");
        for flag in [
            "--offline",
            "-p",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
        ] {
            assert!(args.iter().any(|a| a == flag), "missing {flag} in {args:?}");
        }
        let pair = |k: &str| {
            let i = args
                .iter()
                .position(|a| a == k)
                .unwrap_or_else(|| panic!("no {k}"));
            args[i + 1].clone()
        };
        assert_eq!(pair("--mode"), "json");
        assert_eq!(pair("--provider"), "local-tiel");
        assert_eq!(pair("--model"), "tiel-coder-35b");
        assert_eq!(pair("--tools"), "read,edit,write,bash");
        // The prompt is the LAST argument, after `--`.
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], "do the thing");
    }

    #[test]
    fn a_description_that_opens_with_a_dash_is_still_the_prompt() {
        let prompt = prompt_for(&task("--verbose should not crash the parser."));
        let args = pi_args(DEFAULT_PROVIDER, DEFAULT_MODEL, &prompt);
        assert_eq!(args.last().unwrap(), &prompt);
        assert_eq!(args[args.len() - 2], "--");
    }

    #[test]
    fn steps_count_tool_execution_starts_only() {
        let stream = concat!(
            "{\"type\":\"agent_start\"}\n",
            "{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"toolcall_start\"}}\n",
            "{\"type\":\"tool_execution_start\",\"toolName\":\"read\"}\n",
            "{\"type\":\"tool_execution_end\",\"toolName\":\"read\"}\n",
            "{\"type\":\"tool_execution_start\",\"toolName\":\"bash\"}\n",
            "{\"type\":\"agent_end\"}\n",
        );
        assert_eq!(count_tool_calls(stream), 2);
        assert_eq!(count_tool_calls("OK\n"), 0);
    }

    /// The real thing: pi, on the local model, driven through the unchanged
    /// `run_task`. Needs pi installed and the model server on 11436, so it is
    /// opt-in: `cargo test -p sc-eval --lib pi_arm -- --ignored`.
    #[test]
    #[ignore]
    fn live_pi_drives_even_parity_to_green() {
        use crate::fsutil::TempWorkspace;
        use crate::runner::run_task;

        let fixture = TempWorkspace::new("pi-fixture").unwrap();
        std::fs::write(fixture.path().join("impl.sh"), "is_even() { return 1; }\n").unwrap();
        std::fs::write(
            fixture.path().join("test.sh"),
            ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
        )
        .unwrap();
        let task = EvalTask {
            id: "even".into(),
            description: "Fix is_even in impl.sh so even numbers are reported even.".into(),
            fixture: fixture.path().to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec!["test.sh".into()],
            solution: None,
            tags: Vec::new(),
            timeout_secs: Some(180),
        };
        let out = TempWorkspace::new("pi-logs").unwrap();
        let solver = PiSolver::new("http://localhost:11436/v1", "", &AgentConfig::default())
            .with_out_dir(out.path().to_path_buf());
        assert!(solver.name().starts_with("pi/"), "name: {}", solver.name());
        let result = run_task(&task, &solver);
        let run = solver.last_run().expect("a run record");
        eprintln!(
            "pi {} -> {:?}; {} tool calls, {}",
            solver.version(),
            result.outcome,
            run.steps,
            run.stop_reason
        );
        assert!(result.outcome.is_pass(), "got {:?}", result.outcome);
        assert!(run.steps > 0, "pi made no tool calls");
        assert!(out.path().join("even-pi-1.log").is_file());
    }
}
