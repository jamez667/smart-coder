//! One running plugin: the process, the reader thread, and the writer.
//!
//! Modelled on `sc_win::session::claude`, which has been driving a subprocess and
//! translating its line stream for long enough to have found the traps. Two of them are
//! carried over verbatim and are commented where they appear: **stderr must be drained
//! on its own thread**, and **cancellation kills the process** rather than asking it to
//! stop.
//!
//! What is new here is that the pipe runs both ways. Claude Code streams out and never
//! listens; a plugin has to be written to as well as read from, which is why this holds
//! a `ChildStdin` and why the handshake can time out.

use std::io::{BufRead, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use sc_plugin_proto::{HostMessage, Incoming, Manifest, PluginMessage};

use super::discover::Discovered;

/// How long a plugin gets to answer [`HostMessage::Initialize`].
///
/// Startup is on the critical path — every plugin's handshake happens before the window
/// is usable — so this is short. A plugin that needs longer to become *useful* should
/// answer the handshake promptly and do its slow work afterwards; the handshake is a
/// declaration, not a readiness check.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a plugin gets to exit after [`HostMessage::Shutdown`] before it is killed.
///
/// Deliberately brief. This runs while the user is closing the window, which is the
/// moment they are least willing to wait, and a plugin that ignores shutdown must not
/// be able to hold the editor open.
pub const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// What a running plugin sends back to the UI thread.
///
/// A channel rather than direct mutation for the reason every other background worker
/// here uses one: the reader runs on its own thread, and the UI drains this on tick.
#[derive(Debug)]
pub enum PluginEvent {
    /// The handshake completed.
    Ready(Box<Manifest>),
    /// A message from the plugin.
    Message(Box<PluginMessage>),
    /// The plugin stopped. `Ok` for a clean exit, `Err` with a reason otherwise.
    Stopped(Result<(), String>),
}

/// A plugin process, running.
pub struct Plugin {
    /// Directory name — the identity until the handshake, and the fallback if it fails.
    pub dir_name: String,
    /// The manifest, once the handshake has completed.
    pub manifest: Option<Manifest>,
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    /// Lines the plugin sent that this host could not parse.
    ///
    /// Counted, never fatal. Shown in the Plugins panel so a plugin talking past the
    /// host is visible rather than mysterious — the same instinct as the Claude
    /// driver's skipped-line counter, which exists because a silently halved feed is
    /// worse than a loud failure.
    pub unknown_lines: usize,
    /// The last few log lines, for the Plugins panel.
    pub log: Vec<String>,
    /// How long the handshake took. Worth showing: it is the part of startup a plugin
    /// can make slow.
    pub handshake_ms: Option<u128>,
    events: Receiver<PluginEvent>,
    next_request_id: u64,
}

impl Plugin {
    /// Spawn `plugin` and start its reader thread.
    ///
    /// Returns a `Plugin` whose `manifest` is `None` until [`PluginEvent::Ready`] is
    /// drained — the handshake is asynchronous like everything else, because blocking
    /// startup on a child process is how an editor takes four seconds to open.
    pub fn spawn(plugin: &Discovered) -> Result<Self, String> {
        let mut child = crate::proc::command(&plugin.command)
            .args(&plugin.args)
            .current_dir(&plugin.dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                // "Not installed" is a thing the user can fix and every other spawn
                // failure is not, so they are not the same message. The Claude driver
                // makes exactly this distinction, for exactly this reason.
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!("{} was not found", plugin.command.display())
                } else {
                    format!("could not start {}: {e}", plugin.command.display())
                }
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the plugin started but produced no output stream".to_string())?;
        let stdin = child.stdin.take();

        // stderr on its own thread. Without this a chatty failure fills the pipe buffer
        // and the child blocks forever writing to it — a hang that looks like a stuck
        // plugin, and one that took a while to diagnose the first time it happened to
        // the Claude runner.
        if let Some(err) = child.stderr.take() {
            let name = plugin.dir_name.clone();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                    // Deliberately eprintln rather than a channel: stderr from a plugin
                    // is a developer-facing diagnostic, and routing it through the UI
                    // would make a noisy plugin able to flood the event channel.
                    eprintln!("[plugin {name}] {line}");
                }
            });
        }

        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_lines(stdout, tx));

        Ok(Self {
            dir_name: plugin.dir_name.clone(),
            manifest: None,
            child,
            stdin,
            unknown_lines: 0,
            log: Vec::new(),
            handshake_ms: None,
            events,
            next_request_id: 1,
        })
    }

    /// Send a message. Best-effort: a plugin that has died takes its pipe with it, and
    /// the reader thread will report the exit — so a failed write needs no separate
    /// error path.
    pub fn send(&mut self, msg: &HostMessage) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let line = sc_plugin_proto::to_line(msg);
        if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
            // Drop the handle so later sends are cheap no-ops rather than repeated
            // failures against a dead pipe.
            self.stdin = None;
        }
    }

    /// The next request id for a host→plugin request.
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Drain whatever the plugin has sent since the last call.
    ///
    /// Non-blocking, called on the UI tick, and **batched** — a plugin streaming a feed
    /// pushes faster than the tick drains, so taking one event per tick would fall
    /// steadily further behind. `Session::drain_events` batches for the same reason.
    pub fn drain(&mut self) -> Vec<PluginEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            match &ev {
                PluginEvent::Message(m) => {
                    if let PluginMessage::Log { message } = m.as_ref() {
                        self.push_log(message.clone());
                    }
                }
                PluginEvent::Ready(m) => self.manifest = Some((**m).clone()),
                PluginEvent::Stopped(_) => {}
            }
            out.push(ev);
        }
        out
    }

    /// Record a log line, keeping only the tail.
    ///
    /// Bounded because a plugin logging in a loop would otherwise grow this without
    /// limit, and the Plugins panel only ever shows the end of it.
    pub fn push_log(&mut self, line: String) {
        const MAX_LOG: usize = 200;
        self.log.push(line);
        if self.log.len() > MAX_LOG {
            self.log.drain(..self.log.len() - MAX_LOG);
        }
    }

    /// Whether the process is still alive.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Ask the plugin to stop, then make sure it has.
    ///
    /// Politely first, because a plugin may have state to flush; then by killing the
    /// **tree**, because a plugin that spawned its own children (a language server, a
    /// `git` process) would otherwise leave them orphaned. That is the same reason
    /// `proc::kill_tree` exists at all.
    pub fn shutdown(&mut self) {
        self.send(&HostMessage::Shutdown);
        // Dropping stdin closes the pipe, which is the other half of the signal: a
        // plugin blocked on a read gets EOF even if it ignores the message.
        self.stdin = None;

        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        crate::proc::kill_tree(self.child.id());
        let _ = self.child.kill();
    }
}

/// Shows the fields worth seeing in a panic or a log, and omits the process handle and
/// the channel, which have no useful representation. Hand-written because `Child` and
/// `Receiver` are not `Debug` — and the derived version would be noise even if they were.
impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("dir_name", &self.dir_name)
            .field("id", &self.manifest.as_ref().map(|m| &m.id))
            .field("unknown_lines", &self.unknown_lines)
            .field("handshake_ms", &self.handshake_ms)
            .finish_non_exhaustive()
    }
}

impl Drop for Plugin {
    /// A dropped plugin must not outlive the editor.
    ///
    /// Without this a crash or an early return leaves an orphaned process holding a
    /// workspace lock or a port, which the user then has to find in Task Manager.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            crate::proc::kill_tree(self.child.id());
            let _ = self.child.kill();
        }
    }
}

/// The reader thread: one line at a time, forever, until the pipe closes.
///
/// Translation happens in `sc_plugin_proto::parse_plugin_line`, which is pure and
/// fixture-tested — so this function holds only the parts that genuinely need a
/// process, which is the split that makes the format contract provable at all.
fn read_lines(stdout: std::process::ChildStdout, tx: Sender<PluginEvent>) {
    let mut handshaken = false;
    for line in std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
    {
        if line.trim().is_empty() {
            continue;
        }
        match sc_plugin_proto::parse_plugin_line(&line) {
            Incoming::Message(m) => {
                // The handshake reply is lifted out into its own event so the host can
                // tell "declared its contributions" from "is chattering", and so a
                // second Initialized is ignored rather than silently re-registering
                // panels the layout has already been built against.
                if let PluginMessage::Initialized { manifest } = m.as_ref() {
                    if !handshaken {
                        handshaken = true;
                        if tx
                            .send(PluginEvent::Ready(Box::new(manifest.clone())))
                            .is_err()
                        {
                            return;
                        }
                    }
                    continue;
                }
                if tx.send(PluginEvent::Message(m)).is_err() {
                    // The UI dropped the receiver: the app is closing. Stop reading.
                    return;
                }
            }
            Incoming::Unknown => {
                // Counted by the owner when it drains; not fatal, and not reported as
                // an error. See `Incoming` for why this rule exists.
                if tx
                    .send(PluginEvent::Message(Box::new(PluginMessage::Log {
                        message: format!("[unparsed] {}", truncate(&line, 200)),
                    })))
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = tx.send(PluginEvent::Stopped(Ok(())));
}

/// Shorten a line for the log, so one enormous unparsed line cannot fill the panel.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_lines_alone() {
        assert_eq!(truncate("short", 200), "short");
    }

    /// One enormous unparsed line must not fill the Plugins panel's log.
    #[test]
    fn truncate_shortens_and_marks_long_lines() {
        let long = "x".repeat(500);
        let out = truncate(&long, 200);
        assert_eq!(out.chars().count(), 201, "200 plus the ellipsis");
        assert!(out.ends_with('…'));
    }

    /// Truncation counts CHARACTERS, not bytes: slicing a multi-byte string by byte
    /// index panics, and a plugin logging non-ASCII is not a crash the editor should
    /// take.
    #[test]
    fn truncate_does_not_split_a_multibyte_character() {
        let s = "é".repeat(300);
        let out = truncate(&s, 200);
        assert_eq!(out.chars().count(), 201);
    }

    /// A plugin that cannot be spawned says which program was missing — the message
    /// goes in the Plugins panel, and "failed to start" tells the user nothing they
    /// can act on.
    #[test]
    fn a_missing_program_names_itself() {
        let err = Plugin::spawn(&Discovered {
            dir_name: "ghost".to_string(),
            dir: std::env::temp_dir(),
            command: std::path::PathBuf::from("sc-definitely-not-a-real-program"),
            args: Vec::new(),
            enabled: true,
        })
        .unwrap_err();
        assert!(err.contains("sc-definitely-not-a-real-program"), "{err}");
        assert!(err.contains("not found"), "{err}");
    }
}
