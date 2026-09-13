//! **Claude Code, as a plugin** (spec 25).
//!
//! The first real migration out of the `sc-win` binary, and the easiest one: the format
//! knowledge was already pure ([`stream`]) and the subprocess handling was already
//! isolated, so this is mostly a change of destination. What was `UiEvent` down a channel
//! is now `panel-content` down a pipe.
//!
//! # The shape
//!
//! Two panels, because the content model has no overlay and the ⚙ options menu was a
//! floating card. That is not a workaround — it is the thing spec 25 chose *instead* of
//! adding a layout primitive, and it is better in one way the old menu was not: the
//! options are a panel you can dock wherever you like rather than a card that covers the
//! feed.
//!
//! * **`feed`** — the run: what Claude said, what it called, what answered.
//! * **`options`** — model, permission mode, resume, tool restrictions.
//!
//! # What it needs from the host
//!
//! Almost nothing: the workspace path, which arrives in the handshake. It reads no
//! buffers and edits no files — Claude Code does its own editing, and `Attach` passes a
//! *path* so Claude's own Read tool fetches the content. So the manifest requests no
//! capabilities at all, which is worth noticing: the most demanding panel in the
//! application turns out to need the least from the editor.

mod options;
mod runner;
mod stream;
mod ui;

use std::io::BufRead;
use std::sync::mpsc;

use sc_plugin_proto::{
    to_line, HostMessage, Manifest, Outgoing, PanelDecl, PluginMessage, PROTOCOL_VERSION,
};

use options::Options;

/// What the plugin is doing right now.
struct State {
    /// Where the project is. `None` until the handshake, and again if it closes.
    workspace: Option<std::path::PathBuf>,
    /// The run feed, oldest first.
    feed: Vec<ui::Row>,
    /// The run in flight, if any.
    run: Option<runner::Run>,
    /// Options, persisted beside the manifest.
    opts: Options,
    /// Lines of the stream this build did not understand. Reported at the end of a run
    /// rather than per line: a warning that fires mid-stream is noise, and one that never
    /// fires at all is the format drifting unseen.
    unknown: usize,
}

fn main() {
    let stdin = std::io::stdin();
    let (tx, rx) = mpsc::channel::<runner::Event>();

    let mut state = State {
        workspace: None,
        feed: Vec::new(),
        run: None,
        opts: Options::default(),
        unknown: 0,
    };

    // The host writes one message per line and waits for nothing, so a blocking read here
    // is correct: this loop IS the plugin. Run events arrive on `rx` and are drained after
    // each host message and on a short idle tick, which is what keeps the feed moving
    // while nothing is being typed.
    for line in stdin.lock().lines().map_while(Result::ok) {
        if let Outgoing::Message(msg) = sc_plugin_proto::parse_host_line(&line) {
            if handle(&mut state, *msg, &tx) {
                break;
            }
        }
        drain_run(&mut state, &rx);
    }
}

/// Handle one host message. Returns true to stop.
fn handle(state: &mut State, msg: HostMessage, tx: &mpsc::Sender<runner::Event>) -> bool {
    match msg {
        HostMessage::Initialize { workspace, .. } => {
            state.workspace = workspace.map(std::path::PathBuf::from);
            state.opts = Options::load();
            send(&PluginMessage::Initialized {
                manifest: manifest(),
            });
            // Both panels get their opening content immediately. A panel that stays blank
            // until the first event is indistinguishable from one that is broken.
            push_feed(state);
            push_options(state);
        }

        HostMessage::Shutdown => {
            // Kill the run rather than orphaning it: a `claude` process still editing
            // files after the editor closed is the worst version of this.
            if let Some(run) = state.run.take() {
                run.cancel();
            }
            return true;
        }

        HostMessage::WorkspaceChanged { workspace } => {
            state.workspace = workspace.map(std::path::PathBuf::from);
            // A run belongs to the project it started in. Carrying one across a project
            // switch would have it editing files nobody is looking at.
            if let Some(run) = state.run.take() {
                run.cancel();
                state
                    .feed
                    .push(ui::Row::note("Cancelled — the project changed."));
            }
            push_feed(state);
        }

        // The composer submitted: `task=<what they typed>`.
        HostMessage::PanelEvent { panel, value } if panel == "feed" => {
            let task = value
                .strip_prefix("task=")
                .unwrap_or(&value)
                .trim()
                .to_string();
            start_run(state, task, tx);
        }

        HostMessage::PanelEvent { panel, value } if panel == "options" => {
            state.opts.apply_form(&value);
            state.opts.save();
            push_options(state);
        }

        HostMessage::CommandInvoked { command, args } => {
            command_invoked(state, &command, &args, tx);
        }

        // Every other message is a notification this plugin did not subscribe to, or a
        // response to a request it never made. Ignored, per the protocol's rule.
        _ => {}
    }
    false
}

/// Run one of the declared commands.
fn command_invoked(
    state: &mut State,
    command: &str,
    args: &[String],
    tx: &mpsc::Sender<runner::Event>,
) {
    match command {
        "claude.cancel" => {
            if let Some(run) = state.run.take() {
                run.cancel();
                state
                    .feed
                    .push(ui::Row::note("Cancelled — the process was stopped."));
                push_feed(state);
            }
        }
        "claude.clear" => {
            state.feed.clear();
            state.unknown = 0;
            push_feed(state);
        }
        "claude.cycle-model" => {
            state.opts.cycle_model();
            state.opts.save();
            push_options(state);
        }
        "claude.cycle-permission" => {
            state.opts.cycle_permission();
            state.opts.save();
            push_options(state);
        }
        "claude.toggle-continue" => {
            state.opts.toggle_continue();
            state.opts.save();
            push_options(state);
        }
        "claude.resume" => {
            // The picker chose a specific past conversation; it wins over `--continue`,
            // which only ever means "the most recent".
            state.opts.resume_session = args.first().cloned();
            state.opts.continue_session = false;
            push_options(state);
        }
        "claude.run" => {
            // The same action as submitting the composer, for the command palette.
            let task = args.join(" ");
            start_run(state, task, tx);
        }
        _ => {}
    }
}

/// Start a run, if there is something to run and somewhere to run it.
fn start_run(state: &mut State, task: String, tx: &mpsc::Sender<runner::Event>) {
    if task.is_empty() || state.run.is_some() {
        return;
    }
    let Some(ws) = state.workspace.clone() else {
        state.feed.push(ui::Row::error(
            "Open a project first — Claude Code runs in it.",
        ));
        push_feed(state);
        return;
    };

    // Echo the task, set apart, so scrolling back through a long run shows where each
    // exchange began.
    state.feed.push(ui::Row::you(&task));
    state.unknown = 0;
    state.run = Some(runner::Run::start(&task, &ws, &state.opts, tx.clone()));

    // Clear the composer FIRST, then push the feed. The host holds what the user typed,
    // and without this the task stays in the box and the next Enter sends it again.
    send(&PluginMessage::ClearFields {
        panel: "feed".to_string(),
    });
    push_feed(state);
}

/// Drain whatever the running process has produced.
fn drain_run(state: &mut State, rx: &mpsc::Receiver<runner::Event>) {
    let mut changed = false;
    while let Ok(ev) = rx.try_recv() {
        changed = true;
        match ev {
            runner::Event::Line(line) => match line {
                stream::Line::Event(e) => state.feed.push(ui::Row::from_event(e)),
                stream::Line::Done { ok, summary } => {
                    state.feed.push(ui::Row::finished(ok, &summary));
                    state.run = None;
                }
                stream::Line::Ignored => {}
                stream::Line::Unknown => state.unknown += 1,
            },
            runner::Event::Failed(why) => {
                state.feed.push(ui::Row::error(&why));
                state.run = None;
            }
            runner::Event::Ended => {
                // The process exited without a `result` line. Only worth saying when the
                // feed does not already end in an outcome.
                if state.run.take().is_some() {
                    state.feed.push(ui::Row::note("The process ended."));
                }
            }
        }
    }
    if changed {
        // Reported once, at the end, and only when it happened: a format change that
        // silently halves the feed should be visible, and a warning that fires every run
        // is one nobody reads.
        if state.run.is_none() && state.unknown > 0 {
            let n = state.unknown;
            state.feed.push(ui::Row::note(&format!(
                "{n} unrecognised output line{} skipped.",
                if n == 1 { "" } else { "s" }
            )));
            state.unknown = 0;
        }
        push_feed(state);
    }
}

/// What this plugin contributes.
fn manifest() -> Manifest {
    Manifest {
        id: "claude-code".to_string(),
        name: "Claude Code".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        panels: vec![
            PanelDecl {
                id: "feed".to_string(),
                title: "Claude Code".to_string(),
            },
            PanelDecl {
                id: "options".to_string(),
                title: "Claude Options".to_string(),
            },
        ],
        commands: vec![
            cmd("claude.run", "Run Claude Code"),
            cmd("claude.cancel", "Stop Claude Code"),
            cmd("claude.clear", "Clear the Claude feed"),
            cmd("claude.cycle-model", "Claude: next model"),
            cmd("claude.cycle-permission", "Claude: next permission mode"),
            cmd("claude.toggle-continue", "Claude: continue last session"),
            cmd("claude.resume", "Claude: resume a session"),
        ],
        // NONE. Claude Code reads and edits files itself; `Attach` passes a path and lets
        // its own Read tool fetch the content. The most demanding panel in the app turns
        // out to need the least from the editor.
        capabilities: Vec::new(),
        subscriptions: vec![sc_plugin_proto::manifest::Subscription::WorkspaceChanged],
    }
}

fn cmd(id: &str, title: &str) -> sc_plugin_proto::manifest::CommandDecl {
    sc_plugin_proto::manifest::CommandDecl {
        id: id.to_string(),
        title: title.to_string(),
    }
}

/// Push the feed panel, pinned to its tail.
fn push_feed(state: &State) {
    send(&PluginMessage::PanelContent {
        panel: "feed".to_string(),
        content: ui::feed(&state.feed, state.run.is_some(), state.workspace.is_some()),
        // A streaming feed that does not follow its own tail is unusable — the reason
        // `scroll` exists in v2 at all.
        scroll: Some(sc_plugin_proto::Scroll::Bottom),
    });
}

/// Push the options panel.
fn push_options(state: &State) {
    send(&PluginMessage::PanelContent {
        panel: "options".to_string(),
        content: ui::options(&state.opts, state.workspace.as_deref()),
        scroll: None,
    });
}

/// Write one message to the host.
///
/// Best-effort: if the pipe is gone the host has closed, and the next read returns EOF and
/// ends the loop. There is nothing useful to do with the error in between.
fn send(msg: &PluginMessage) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(to_line(msg).as_bytes());
    let _ = out.flush();
}
