//! Driving the `claude` CLI: spawn, read, cancel.
//!
//! Ported from `sc_win::session::claude`, which found the traps already. Two are carried
//! over and commented where they appear, because both are the kind of bug that presents
//! as a mystery:
//!
//! * **stderr is drained on its own thread.** A chatty failure otherwise fills the pipe
//!   buffer and the child blocks forever writing to it — a hang that looks like a stuck
//!   run.
//! * **Cancel kills the process.** A cooperative flag has nothing to check it: a
//!   subprocess has no turn boundary, and a cancel that left an orphaned `claude` still
//!   editing files would be worse than no cancel button, because the user would believe
//!   they had stopped it.
//!
//! What is new: this process is not the editor. Killing the child on drop matters more
//! here, because the plugin can be shut down by the host at any time.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::options::{args, Options};
use crate::stream;

/// Something the run produced.
pub enum Event {
    /// One translated line.
    Line(stream::Line),
    /// The run could not start, or ended badly.
    Failed(String),
    /// The stream closed.
    Ended,
}

/// A run in flight.
pub struct Run {
    cancel: Arc<AtomicBool>,
    /// The child's pid, for the kill. Held rather than the `Child` itself because the
    /// reader thread owns that.
    pid: Arc<std::sync::Mutex<Option<u32>>>,
}

impl Run {
    /// Spawn `claude` over `task` in `workspace`, streaming translated lines to `tx`.
    ///
    /// Returns immediately; the work happens on a reader thread. A spawn failure arrives
    /// as [`Event::Failed`] like any other, rather than as a `Result` here — the caller
    /// has one place to render a problem and this keeps it that way.
    pub fn start(
        task: &str,
        workspace: &std::path::Path,
        opts: &Options,
        tx: Sender<Event>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let pid: Arc<std::sync::Mutex<Option<u32>>> = Arc::new(std::sync::Mutex::new(None));

        let argv = args(task, opts);
        let ws = workspace.to_path_buf();
        let cancel_thread = cancel.clone();
        let pid_thread = pid.clone();

        std::thread::spawn(move || {
            let mut child = match std::process::Command::new("claude")
                .args(&argv)
                .current_dir(&ws)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    // "Not installed" is a thing the user can fix, and every other spawn
                    // failure is not. Saying "program not found" alone does not tell them
                    // how.
                    let msg = if e.kind() == std::io::ErrorKind::NotFound {
                        "Claude Code is not installed, or `claude` is not on PATH.".to_string()
                    } else {
                        format!("Could not start Claude Code: {e}")
                    };
                    let _ = tx.send(Event::Failed(msg));
                    return;
                }
            };
            *pid_thread.lock().unwrap() = Some(child.id());

            // stderr on its own thread, capped: a chatty failure otherwise fills the pipe
            // buffer and the child blocks forever writing to it.
            let stderr = child.stderr.take().map(|err| {
                std::thread::spawn(move || {
                    let mut buf = String::new();
                    for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                        if buf.len() < 4096 {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                    }
                    buf
                })
            });

            let Some(stdout) = child.stdout.take() else {
                let _ = tx.send(Event::Failed(
                    "Claude Code started but produced no output stream.".to_string(),
                ));
                let _ = child.kill();
                return;
            };

            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if cancel_thread.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    return;
                }
                for parsed in stream::parse_line_in(&line, &ws) {
                    if tx.send(Event::Line(parsed)).is_err() {
                        // The plugin is shutting down. Take the child with it rather than
                        // leaving it editing files nobody is watching.
                        let _ = child.kill();
                        return;
                    }
                }
            }

            // The stream closed. A non-zero exit with no `result` line is a failure worth
            // naming, and stderr is the only place that says why.
            let status = child.wait().ok();
            let bad = status.map(|s| !s.success()).unwrap_or(false);
            if bad {
                let detail = stderr
                    .and_then(|h| h.join().ok())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "no output".to_string());
                let _ = tx.send(Event::Failed(format!(
                    "Claude Code exited without finishing: {}",
                    detail.trim()
                )));
            } else {
                let _ = tx.send(Event::Ended);
            }
        });

        Self { cancel, pid }
    }

    /// Stop the run.
    ///
    /// Sets the flag AND kills the process. The flag alone only takes effect on the next
    /// line, and a run waiting on a slow model produces no lines for a long time — so a
    /// cancel that only set a flag would look like it had not worked.
    pub fn cancel(self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(pid) = *self.pid.lock().unwrap() {
            kill(pid);
        }
    }
}

/// Kill a process tree by pid.
///
/// The TREE, not the process: `claude` spawns its own children (a shell for `Bash`, a
/// language server), and killing only the parent orphans them. That is the same reason
/// the editor has `proc::kill_tree`; this crate cannot depend on it, so it is restated
/// here — nine lines, and the alternative is a dependency on the whole editor.
fn kill(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(not(windows))]
    {
        // The negative pid signals the process GROUP.
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .output();
    }
}
