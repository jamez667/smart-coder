//! [`Terminal`] — a VS-Code-style command-runner panel for the bottom strip.
//!
//! You type a command, it runs via [`crate::proc`] (windowless on Windows) in the
//! open workspace, and its stdout/stderr stream **live** into a scrollback pane; the
//! exit code closes each run. This is a *command runner*, not a PTY: no interactive
//! input into the child, no ANSI colour/cursor handling. That keeps it dependency-free
//! and fits the app's existing "background thread → `std::mpsc` → drain on `Tick`"
//! streaming pattern (see [`crate::session`]).
//!
//! Nothing here is an iced type, so the parse/spawn/drain flow is host-testable — the
//! renderer in `app.rs` only lays out [`Terminal::lines`] and binds the input box.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use sc_verify::SessionContainer;

/// Where a terminal command runs. The app chooses this from its sandbox config: `Host` runs
/// on the local machine via the platform shell; `Container` runs `docker exec` into the
/// workspace's persistent session container (the sandbox), so neither the user's typed
/// commands nor (later) the agent's touch the host.
#[derive(Debug, Clone)]
pub enum ExecMode {
    /// Run on the host through the platform shell, from `cwd`.
    Host { cwd: PathBuf },
    /// Run inside the workspace's persistent sandbox container.
    Container(SessionContainer),
}

impl ExecMode {
    /// Build the OS [`Command`] that runs `cmdline` under this mode. Pure — no spawn — so the
    /// host-vs-container argument construction is testable. The whole line is handed to a
    /// shell (`cmd`/`sh -c`, or `sh -c` inside the container) so built-ins, pipes, `PATH`,
    /// and `&&`/`||` all behave like a real terminal.
    pub fn build(&self, cmdline: &str) -> Command {
        match self {
            ExecMode::Host { cwd } => {
                #[cfg(windows)]
                let mut c = {
                    // /D skips AutoRun registry commands; /C runs the string then exits.
                    let mut c = crate::proc::command("cmd");
                    c.args(["/D", "/C", cmdline]);
                    c
                };
                #[cfg(not(windows))]
                let mut c = {
                    let mut c = crate::proc::command("sh");
                    c.args(["-c", cmdline]);
                    c
                };
                c.current_dir(cwd);
                c
            }
            // `docker exec` already sets `-w /workspace`; no host cwd applies. Routed through
            // `proc::` isn't needed (docker's own client shows no console), but the constructed
            // Command carries no console-suppression flag here — acceptable, docker is quiet.
            ExecMode::Container(sc) => sc.exec_command(cmdline),
        }
    }
}

/// Which stream a scrollback line came from, so the renderer can colour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Child stdout — normal foreground text.
    Stdout,
    /// Child stderr — rendered as a warning/error colour.
    Stderr,
    /// App-generated: the echoed `$ cmd` header and the `[exit N]` footer.
    Meta,
}

/// One line of scrollback: its text (no trailing newline) and originating [`Stream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermLine {
    pub text: String,
    pub stream: Stream,
}

impl TermLine {
    fn new(stream: Stream, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            stream,
        }
    }
}

/// A message from the reader/waiter threads of a running command back to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermMsg {
    /// One line of output from the given stream.
    Line(Stream, String),
    /// The child exited with this status code (`None` = terminated by signal / unknown).
    Exited(Option<i32>),
}

/// The terminal panel state: the input box, scrollback, command history, and whether a
/// command is currently running (its channel is held by the app, not here).
#[derive(Debug, Default)]
pub struct Terminal {
    /// The current contents of the input box.
    pub input: String,
    /// The scrollback, oldest first.
    pub lines: Vec<TermLine>,
    /// Whether a command is running right now (input is disabled/Kill shown while true).
    pub running: bool,
    /// Submitted command lines, oldest first, for Up/Down recall.
    history: Vec<String>,
    /// Recall cursor: `None` = editing fresh; `Some(i)` = showing `history[i]`.
    hist_cursor: Option<usize>,
    /// OS process id of the running child, for [`Terminal::kill`]. `None` when idle.
    child_pid: Option<u32>,
}

/// Cap the scrollback so a chatty command (a full `cargo build`) can't grow the buffer
/// without bound. Oldest lines are dropped once over the cap.
const MAX_LINES: usize = 5000;

impl Terminal {
    /// Spawn `cmdline` streaming its output, under the given [`ExecMode`] — the host shell
    /// (VS-Code-style `cmd`/`sh -c`, built-ins/pipes/PATH all work) or `docker exec` into the
    /// workspace's persistent sandbox container (so nothing touches the host). Echoes a `$ cmd`
    /// header, records the command in history, flips `running`, and returns the receiver the
    /// app drains on each tick (via [`Terminal::apply`]). Returns `None` (with an error line +
    /// `[exit -1]` footer already pushed) if the line is blank or the process fails to spawn —
    /// so the caller never has a dangling `running` with no channel.
    pub fn run(&mut self, cmdline: &str, mode: &ExecMode) -> Option<Receiver<TermMsg>> {
        let trimmed = cmdline.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Echo the command and remember it, regardless of whether the spawn succeeds.
        self.lines.push(TermLine::new(Stream::Meta, format!("$ {trimmed}")));
        if self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
        }
        self.hist_cursor = None;
        self.input.clear();

        let mut cmd = mode.build(trimmed);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.lines
                    .push(TermLine::new(Stream::Stderr, format!("failed to start command: {e}")));
                self.lines.push(TermLine::new(Stream::Meta, "[exit -1]"));
                self.trim();
                return None;
            }
        };
        self.child_pid = Some(child.id());

        let (tx, rx) = mpsc::channel();
        // One reader thread per stream, each pumping lines onto the shared channel.
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, Stream::Stdout, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, Stream::Stderr, tx.clone());
        }
        // Waiter thread: block on the child, then report the exit code. Owns the last `tx`
        // so the channel stays open until the process is truly done.
        thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            let _ = tx.send(TermMsg::Exited(code));
        });

        self.running = true;
        self.trim();
        Some(rx)
    }

    /// Drain every currently-queued message from a run's receiver into the scrollback.
    /// Returns `true` once the run has finished (an `Exited` was seen), so the caller can
    /// drop the receiver and stop ticking for it. Safe to call every tick.
    pub fn drain(&mut self, rx: &Receiver<TermMsg>) -> bool {
        let mut finished = false;
        while let Ok(msg) = rx.try_recv() {
            if self.apply(msg) {
                finished = true;
            }
        }
        self.trim();
        finished
    }

    /// Apply a single message. Returns `true` if it was the `Exited` terminator (which also
    /// clears `running` and pushes the `[exit N]` footer). Split out so it's unit-testable
    /// without threads/channels.
    pub fn apply(&mut self, msg: TermMsg) -> bool {
        match msg {
            TermMsg::Line(stream, text) => {
                self.lines.push(TermLine::new(stream, text));
                false
            }
            TermMsg::Exited(code) => {
                let footer = match code {
                    Some(c) => format!("[exit {c}]"),
                    None => "[exit ?]".to_string(),
                };
                self.lines.push(TermLine::new(Stream::Meta, footer));
                self.running = false;
                self.child_pid = None;
                true
            }
        }
    }

    /// Recall the previous command (Up). No-op when history is empty.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.hist_cursor {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_cursor = Some(next);
        self.input = self.history[next].clone();
    }

    /// Recall the next command (Down); walking past the newest clears back to a fresh line.
    pub fn history_next(&mut self) {
        let Some(i) = self.hist_cursor else { return };
        if i + 1 >= self.history.len() {
            self.hist_cursor = None;
            self.input.clear();
        } else {
            self.hist_cursor = Some(i + 1);
            self.input = self.history[i + 1].clone();
        }
    }

    /// Terminate the running child (and its process tree). No-op when nothing is running.
    /// The child's stream-close then delivers the normal `Exited` terminator through the
    /// channel, which clears `running`/`child_pid` and pushes the `[exit]` footer — so the
    /// UI state converges the same way a natural exit does. Best-effort: a kill that fails to
    /// spawn is reported as a stderr line.
    pub fn kill(&mut self) {
        let Some(pid) = self.child_pid else { return };
        self.lines
            .push(TermLine::new(Stream::Meta, "^C  (terminating)"));
        #[cfg(windows)]
        let mut cmd = {
            let mut c = crate::proc::command("taskkill");
            c.args(["/PID", &pid.to_string(), "/T", "/F"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = crate::proc::command("kill");
            c.args(["-TERM", &pid.to_string()]);
            c
        };
        if let Err(e) = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            self.lines
                .push(TermLine::new(Stream::Stderr, format!("kill failed: {e}")));
        }
    }

    /// Push an app-generated informational line (sandbox status, warnings) into the
    /// scrollback, styled as [`Stream::Meta`].
    pub fn note(&mut self, text: impl Into<String>) {
        self.lines.push(TermLine::new(Stream::Meta, text.into()));
        self.trim();
    }

    /// Record a command that was **refused** (not run) because the sandbox was intended but
    /// unavailable — echoes the `$ cmd`, the `reason` as an error line, and a `[blocked]`
    /// footer, and stores it in history + clears the input, exactly as a real run would (minus
    /// the execution). Keeps strict-containment refusals visible and re-runnable.
    pub fn blocked(&mut self, cmdline: &str, reason: &str) {
        self.lines.push(TermLine::new(Stream::Meta, format!("$ {cmdline}")));
        self.lines
            .push(TermLine::new(Stream::Stderr, format!("✗ {reason}")));
        self.lines.push(TermLine::new(Stream::Meta, "[blocked]"));
        if self.history.last().map(String::as_str) != Some(cmdline) {
            self.history.push(cmdline.to_string());
        }
        self.hist_cursor = None;
        self.input.clear();
        self.trim();
    }

    /// Clear the scrollback (the `clear`/Ctrl-L affordance). History is kept.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Drop the oldest lines once the buffer exceeds [`MAX_LINES`].
    fn trim(&mut self) {
        let overflow = self.lines.len().saturating_sub(MAX_LINES);
        if overflow > 0 {
            self.lines.drain(0..overflow);
        }
    }
}

/// Spawn a thread that reads `src` line-by-line and forwards each line onto `tx` tagged
/// with `stream`. Ends (dropping its `tx` clone) when the stream closes at child exit.
fn spawn_reader<R>(src: R, stream: Stream, tx: Sender<TermMsg>)
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(src);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(TermMsg::Line(stream, l)).is_err() {
                        break; // UI dropped the receiver — stop reading.
                    }
                }
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn host_mode() -> ExecMode {
        ExecMode::Host {
            cwd: PathBuf::from("."),
        }
    }

    fn parts(cmd: &Command) -> (String, Vec<String>) {
        (
            cmd.get_program().to_string_lossy().into_owned(),
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn host_mode_wraps_whole_line_for_the_shell() {
        // The user's entire line is passed as ONE argument to the shell — so built-ins,
        // pipes, and operators survive intact rather than being tokenised away.
        let line = r#"dir & echo "a b" | findstr b"#;
        let (prog, args) = parts(&host_mode().build(line));
        if cfg!(windows) {
            assert_eq!(prog, "cmd");
            assert_eq!(args, vec!["/D", "/C", line]);
        } else {
            assert_eq!(prog, "sh");
            assert_eq!(args, vec!["-c", line]);
        }
    }

    #[test]
    fn container_mode_execs_into_the_session() {
        let sc = SessionContainer::new(Path::new("/tmp/ws"), "img");
        let (prog, args) = parts(&ExecMode::Container(sc.clone()).build("cargo build"));
        assert_eq!(prog, "docker");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&sc.name().to_string()), "targets our container");
        assert_eq!(args.last().unwrap(), "cargo build");
    }

    #[test]
    fn blocked_records_refusal_without_running() {
        let mut t = Terminal::default();
        t.input = "rm -rf build".into();
        t.blocked("rm -rf build", "no project open");
        assert!(!t.running, "blocked never starts a process");
        assert_eq!(t.input, "", "input cleared");
        let texts: Vec<&str> = t.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["$ rm -rf build", "✗ no project open", "[blocked]"]);
        // Still recallable via history.
        t.history_prev();
        assert_eq!(t.input, "rm -rf build");
    }

    #[test]
    fn run_blank_line_is_none() {
        let mut t = Terminal::default();
        assert!(t.run("   ", &host_mode()).is_none());
        assert!(t.lines.is_empty());
        assert!(!t.running);
    }

    #[test]
    fn run_executes_a_shell_builtin_end_to_end() {
        // `echo` is a shell built-in (no echo.exe on Windows) — the exact class of command
        // that failed with a direct CreateProcess. Going through the shell, it must run and
        // exit 0, with the echoed text landing on stdout.
        let mut t = Terminal::default();
        let rx = t
            .run("echo terminal_ok", &host_mode())
            .expect("shell should spawn");
        // Pump to completion (bounded so a hang can't wedge the test).
        let mut done = false;
        for _ in 0..500 {
            if t.drain(&rx) {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done, "command never reported exit");
        assert!(!t.running);
        let out: String = t
            .lines
            .iter()
            .filter(|l| l.stream == Stream::Stdout)
            .map(|l| l.text.as_str())
            .collect();
        assert!(out.contains("terminal_ok"), "stdout was: {out:?}");
        assert_eq!(t.lines.last().unwrap().text, "[exit 0]");
    }

    #[test]
    fn apply_builds_lines_and_exit_footer() {
        let mut t = Terminal::default();
        assert!(!t.apply(TermMsg::Line(Stream::Stdout, "hello".into())));
        assert!(!t.apply(TermMsg::Line(Stream::Stderr, "oops".into())));
        assert!(t.running == false); // never set true by apply alone
        let finished = t.apply(TermMsg::Exited(Some(0)));
        assert!(finished);
        assert_eq!(
            t.lines,
            vec![
                TermLine::new(Stream::Stdout, "hello"),
                TermLine::new(Stream::Stderr, "oops"),
                TermLine::new(Stream::Meta, "[exit 0]"),
            ]
        );
    }

    #[test]
    fn apply_exit_clears_running_and_reports_nonzero() {
        let mut t = Terminal::default();
        t.running = true;
        t.apply(TermMsg::Exited(Some(101)));
        assert!(!t.running);
        assert_eq!(t.lines.last().unwrap().text, "[exit 101]");
    }

    #[test]
    fn apply_exit_unknown_code() {
        let mut t = Terminal::default();
        t.apply(TermMsg::Exited(None));
        assert_eq!(t.lines.last().unwrap().text, "[exit ?]");
    }

    #[test]
    fn history_nav_walks_back_and_forth() {
        let mut t = Terminal::default();
        t.history = vec!["one".into(), "two".into(), "three".into()];
        t.history_prev(); // newest
        assert_eq!(t.input, "three");
        t.history_prev();
        assert_eq!(t.input, "two");
        t.history_prev();
        assert_eq!(t.input, "one");
        t.history_prev(); // clamps at oldest
        assert_eq!(t.input, "one");
        t.history_next();
        assert_eq!(t.input, "two");
        t.history_next();
        assert_eq!(t.input, "three");
        t.history_next(); // past newest → fresh line
        assert_eq!(t.input, "");
        assert_eq!(t.hist_cursor, None);
    }

    #[test]
    fn history_next_without_recall_is_noop() {
        let mut t = Terminal::default();
        t.input = "typing".into();
        t.history_next();
        assert_eq!(t.input, "typing");
    }

    #[test]
    fn trim_caps_scrollback() {
        let mut t = Terminal::default();
        for i in 0..(MAX_LINES + 10) {
            t.lines.push(TermLine::new(Stream::Stdout, i.to_string()));
        }
        t.trim();
        assert_eq!(t.lines.len(), MAX_LINES);
        // Oldest dropped: first surviving line is "10".
        assert_eq!(t.lines.first().unwrap().text, "10");
    }
}
