//! **The agent, as a plugin** (spec 25).
//!
//! The protocol loop. Chat, the runs, the swarm and the review gates, in their own
//! process, speaking line-delimited JSON to an editor that can no longer compile any of
//! it.
//!
//! # The one thing this loop must never get wrong
//!
//! A [`Pending`] carries a `Sender` the **worker thread is blocked on**. `ChannelGate`
//! sits in `reply_rx.recv()` until someone answers, and falls back to `Abort` only if the
//! channel drops. So a pending request that is received and then forgotten does not fail
//! the run — it hangs it, which is the worst shape a bug can take here.
//!
//! Hence: every `Pending` drained goes into [`State::asks`] and stays there until it is
//! answered. The approvals panel renders them, the answer command resolves one by index,
//! and shutdown drops them all deliberately so the workers unblock and abort rather than
//! leaking threads.
//!
//! # What it needs from the host
//!
//! Nothing but the workspace path, which arrives in the handshake. The agent reads and
//! writes files through its own tools — it always did, as a separate concern from the
//! editor's buffers — so it requests no capabilities. Buffer edits through the protocol
//! are a later step, once the run kinds that want them exist.

use std::io::BufRead;
use std::sync::mpsc::Sender;

use sc_plugin_proto::manifest::{CommandDecl, Subscription};
use sc_plugin_proto::{
    to_line, HostMessage, Manifest, Outgoing, PanelDecl, PluginMessage, PROTOCOL_VERSION,
};

use sc_core::Confirmation;
use sc_plugin_agent::board::SwarmBoard;
use sc_plugin_agent::plan::Plan;
use sc_plugin_agent::view::{agent_rows, swarm_rows, Row};
use sc_plugin_agent::{ui, Pending, RunKind, Session, UiConfig, UiEvent};
use sc_workflow::Decision;

const RUN: &str = "run";
const APPROVALS: &str = "approvals";
const PLAN: &str = "plan";
const BOARD: &str = "board";

/// One decision a blocked worker is waiting on.
struct Ask {
    kind: AskKind,
}

enum AskKind {
    Confirm {
        command: String,
        reason: String,
        reply: Sender<Confirmation>,
    },
    Gate {
        phase: sc_workflow::Phase,
        content: String,
        reply: Sender<Decision>,
    },
}

/// What the plugin is doing right now.
struct State {
    workspace: Option<std::path::PathBuf>,
    /// The run in flight. `Some` is what "running" means.
    session: Option<Session>,
    /// The activity stream, oldest first.
    rows: Vec<Row>,
    /// Which run kind the composer launches.
    kind: RunKind,
    /// Blocked workers, by index. **Never dropped except deliberately** — see the module
    /// header.
    asks: Vec<Option<Ask>>,
    /// The staged workflow's artifacts, when a run produces them.
    plan: Plan,
    /// The swarm's per-subtask board. A swarm's workers interleave in the feed; this
    /// is the same events folded by subtask id so each one has a current status.
    board: SwarmBoard,
}

fn main() {
    let stdin = std::io::stdin();
    let mut state = State {
        workspace: None,
        session: None,
        rows: Vec::new(),
        kind: RunKind::Iterate,
        asks: Vec::new(),
        plan: Plan::default(),
        board: SwarmBoard::default(),
    };

    // The host writes one message per line and waits for nothing, so a blocking read is
    // correct: this loop IS the plugin. The run's events and its blocked requests are
    // drained after every host message.
    for line in stdin.lock().lines().map_while(Result::ok) {
        if let Outgoing::Message(msg) = sc_plugin_proto::parse_host_line(&line) {
            if handle(&mut state, *msg) {
                break;
            }
        }
        drain(&mut state);
    }
}

/// Handle one host message. Returns true to stop.
fn handle(state: &mut State, msg: HostMessage) -> bool {
    match msg {
        HostMessage::Initialize { workspace, .. } => {
            state.workspace = workspace.map(std::path::PathBuf::from);
            send(&PluginMessage::Initialized {
                manifest: manifest(),
            });
            push_all(state);
        }

        HostMessage::Shutdown => {
            // Cancel first, then drop the asks: dropping a reply sender makes the blocked
            // worker take its own fallback (Deny / Abort) and unwind, rather than sitting
            // on a channel nobody will ever write to.
            if let Some(s) = state.session.take() {
                s.cancel();
            }
            state.asks.clear();
            return true;
        }

        HostMessage::WorkspaceChanged { workspace } => {
            state.workspace = workspace.map(std::path::PathBuf::from);
            // A run belongs to the project it started in. Carrying one across a switch
            // would have it editing files nobody is looking at.
            if let Some(s) = state.session.take() {
                s.cancel();
                state
                    .rows
                    .push(Row::err("⚠", "cancelled — the project changed"));
            }
            state.asks.clear();
            state.plan = Plan::default();
            state.board = SwarmBoard::default();
            push_all(state);
        }

        HostMessage::PanelEvent { panel, value } if panel == RUN => {
            // Submitting while a run is live means Stop — the button says so.
            if state.session.is_some() {
                stop(state);
                return false;
            }
            let task = value
                .strip_prefix("task=")
                .unwrap_or(&value)
                .trim()
                .to_string();
            start(state, task);
        }

        HostMessage::CommandInvoked { command, args } => command_invoked(state, &command, &args),

        _ => {}
    }
    false
}

fn command_invoked(state: &mut State, command: &str, args: &[String]) {
    match command {
        "agent.run" => {
            let task = args.join(" ");
            start(state, task);
        }
        "agent.stop" => stop(state),
        "agent.clear" => {
            state.rows.clear();
            push_run(state);
        }
        "agent.set-kind" => {
            if state.session.is_some() {
                return; // Changing mid-run would not affect the run in flight.
            }
            if let Some(k) = args.first().and_then(|s| kind_from_label(s)) {
                state.kind = k;
                push_run(state);
            }
        }
        "agent.answer" => answer(state, args),
        _ => {}
    }
}

/// Answer one blocked worker: `[id, verdict]`.
fn answer(state: &mut State, args: &[String]) {
    let Some(id) = args.first().and_then(|s| s.parse::<usize>().ok()) else {
        return;
    };
    let Some(slot) = state.asks.get_mut(id) else {
        return;
    };
    // Taken, not borrowed: an ask is answered exactly once, and the send consumes the
    // sender. A second click on a stale panel finds `None` and does nothing.
    let Some(ask) = slot.take() else { return };
    let verdict = args.get(1).map(String::as_str).unwrap_or("");

    match ask.kind {
        AskKind::Confirm { command, reply, .. } => {
            let c = match verdict {
                "allow-once" => Confirmation::AllowOnce,
                "allow-remember" => Confirmation::AllowRemember {
                    prefix: prefix_of(&command),
                },
                // Anything unrecognised denies. A misread verdict must not run a command
                // the user did not approve.
                _ => Confirmation::Deny("denied from the approvals panel".to_string()),
            };
            let allowed = !matches!(c, Confirmation::Deny(_));
            let _ = reply.send(c);
            state.rows.push(if allowed {
                Row::ok("✓", format!("approved  {command}"))
            } else {
                Row::err("✕", format!("denied  {command}"))
            });
        }
        AskKind::Gate { phase, reply, .. } => {
            let d = match verdict {
                "approve" => Decision::Approve,
                "revise" => Decision::Revise,
                _ => Decision::Abort,
            };
            state
                .rows
                .push(Row::ok("⏸", format!("{phase:?}: {verdict}")));
            let _ = reply.send(d);
        }
    }
    push_all(state);
}

/// Start a run, if there is something to run and somewhere to run it.
fn start(state: &mut State, task: String) {
    if task.is_empty() || state.session.is_some() {
        return;
    }
    let Some(ws) = state.workspace.clone() else {
        state.rows.push(Row::err(
            "⚠",
            "open a project first — the agent works in it",
        ));
        push_run(state);
        return;
    };

    // Loaded per run, not at startup: a user who fixes an endpoint or a key should not
    // have to restart the editor for the plugin to notice. `resolve_stages` flattens the
    // connection routing into the scalars the backend builders read.
    let mut cfg = UiConfig::load();
    cfg.resolve_stages();

    state.rows.push(Row::ok("❯", task.clone()));
    state.plan = Plan::default();
    state.board = SwarmBoard::default();
    state.session = Some(Session::spawn(state.kind, cfg, task, ws));

    // Clear the composer FIRST, or the task stays in the box and the next Enter sends it
    // again.
    send(&PluginMessage::ClearFields {
        panel: RUN.to_string(),
    });
    push_all(state);
}

fn stop(state: &mut State) {
    if let Some(s) = state.session.take() {
        s.cancel();
        state.rows.push(Row::err("■", "cancelled"));
    }
    // Whatever was blocked is moot now; dropping unblocks those workers.
    state.asks.clear();
    push_all(state);
}

/// Take whatever the run has produced, and whatever it is blocked on.
fn drain(state: &mut State) {
    let Some(session) = &state.session else {
        return;
    };
    let events = session.drain_events();
    let pendings = session.drain_pending();
    if events.is_empty() && pendings.is_empty() {
        return;
    }

    let mut ended = false;
    for ev in events {
        match ev {
            UiEvent::Agent(e) => state.rows.extend(agent_rows(&e)),
            UiEvent::Swarm(e) => {
                // Both, deliberately: the feed narrates what happened in order, the
                // board answers "where is each subtask now". Neither replaces the
                // other, and the fold is cheap.
                state.board.apply(&e);
                state.rows.extend(swarm_rows(&e));
            }
            UiEvent::Phase {
                phase,
                content,
                tests_written,
                dir,
            } => {
                state
                    .plan
                    .apply(phase, &content, &tests_written, dir.as_deref());
                state.rows.push(Row::ok("◆", format!("{phase:?} written")));
            }
            UiEvent::Done { ok, summary } => {
                state.rows.push(if ok {
                    Row::ok("✓", summary)
                } else {
                    Row::err("✗", summary)
                });
                ended = true;
            }
            UiEvent::Failed(why) => {
                state.rows.push(Row::err("✗", why));
                ended = true;
            }
        }
    }

    for p in pendings {
        let kind = match p {
            Pending::Confirm {
                command,
                default_reason,
                reply,
            } => AskKind::Confirm {
                command,
                reason: default_reason,
                reply,
            },
            Pending::Gate {
                phase,
                content,
                reply,
            } => AskKind::Gate {
                phase,
                content,
                reply,
            },
        };
        state.asks.push(Some(Ask { kind }));
    }

    if ended {
        state.session = None;
    }
    push_all(state);
}

fn push_all(state: &State) {
    push_run(state);
    push_approvals(state);
    push_plan(state);
    push_board(state);
}

fn push_run(state: &State) {
    send(&PluginMessage::PanelContent {
        panel: RUN.to_string(),
        content: ui::run_panel(
            &state.rows,
            state.session.is_some(),
            state.workspace.is_some(),
            kind_label(state.kind),
        ),
        // A streaming feed that does not follow its own tail is unusable.
        scroll: Some(sc_plugin_proto::Scroll::Bottom),
    });
}

fn push_approvals(state: &State) {
    let asks: Vec<ui::Ask> = state
        .asks
        .iter()
        .enumerate()
        .filter_map(|(id, slot)| {
            let ask = slot.as_ref()?;
            Some(ui::Ask {
                id,
                kind: match &ask.kind {
                    AskKind::Confirm {
                        command, reason, ..
                    } => ui::AskKind::Confirm {
                        command: command.clone(),
                        reason: reason.clone(),
                    },
                    AskKind::Gate { phase, content, .. } => ui::AskKind::Gate {
                        phase: format!("{phase:?}"),
                        excerpt: excerpt(content),
                    },
                },
            })
        })
        .collect();
    send(&PluginMessage::PanelContent {
        panel: APPROVALS.to_string(),
        content: ui::approvals_panel(&asks),
        scroll: None,
    });
}

fn push_plan(state: &State) {
    let steps: Vec<(String, bool)> = state
        .plan
        .steps()
        .iter()
        .map(|s| (format!("{:?}", s.phase), s.done))
        .collect();
    send(&PluginMessage::PanelContent {
        panel: PLAN.to_string(),
        content: ui::plan_panel(&steps),
        scroll: None,
    });
}

fn push_board(state: &State) {
    send(&PluginMessage::PanelContent {
        panel: BOARD.to_string(),
        content: ui::board_panel(state.board.rows(), state.session.is_some()),
        scroll: None,
    });
}

/// The first slice of an artifact, for the gate card.
///
/// A phase artifact runs to thousands of words; the whole of it in an approval panel
/// buries the buttons. The file itself is already on disk — `Revise` exists precisely
/// because reading and editing it there is the real review.
fn excerpt(content: &str) -> String {
    const MAX: usize = 1200;
    let trimmed = content.trim();
    match trimmed.char_indices().nth(MAX) {
        None => trimmed.to_string(),
        Some((i, _)) => format!(
            "{}…\n\n*(truncated — the full artifact is on disk)*",
            &trimmed[..i]
        ),
    }
}

fn prefix_of(command: &str) -> String {
    match command.find(' ') {
        Some(i) => command[..=i].to_string(),
        None => command.to_string(),
    }
}

fn kind_label(k: RunKind) -> &'static str {
    match k {
        RunKind::Agent => "Agent",
        RunKind::Swarm => "Swarm",
        RunKind::Tdd => "TDD",
        RunKind::SequentialBuild => "Sequential build",
        RunKind::Iterate => "Iterate",
        RunKind::Plan => "Plan",
        RunKind::StagedBuild => "Staged build",
    }
}

fn kind_from_label(s: &str) -> Option<RunKind> {
    Some(match s {
        "Agent" => RunKind::Agent,
        "Swarm" => RunKind::Swarm,
        "TDD" => RunKind::Tdd,
        "Sequential build" => RunKind::SequentialBuild,
        "Iterate" => RunKind::Iterate,
        "Plan" => RunKind::Plan,
        "Staged build" => RunKind::StagedBuild,
        _ => return None,
    })
}

/// What this plugin contributes.
fn manifest() -> Manifest {
    Manifest {
        id: "agent".to_string(),
        name: "Agent".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        panels: vec![
            PanelDecl {
                id: RUN.to_string(),
                title: "Agent".to_string(),
            },
            PanelDecl {
                id: APPROVALS.to_string(),
                title: "Approvals".to_string(),
            },
            PanelDecl {
                id: PLAN.to_string(),
                title: "Plan".to_string(),
            },
            PanelDecl {
                id: BOARD.to_string(),
                title: "Swarm board".to_string(),
            },
        ],
        commands: vec![
            cmd("agent.run", "Agent: run"),
            cmd("agent.stop", "Agent: stop"),
            cmd("agent.clear", "Agent: clear the feed"),
            cmd("agent.set-kind", "Agent: choose run kind"),
            cmd("agent.answer", "Agent: answer a pending decision"),
        ],
        // NONE, and deliberately: the agent reads and writes files through its own tools,
        // which is how it always worked. Spec 25's commitment is that the agent gets no
        // privileged channel — it waits for the same capabilities as any other plugin.
        capabilities: Vec::new(),
        subscriptions: vec![Subscription::WorkspaceChanged],
    }
}

fn cmd(id: &str, title: &str) -> CommandDecl {
    CommandDecl {
        id: id.to_string(),
        title: title.to_string(),
    }
}

/// Write one message to the host.
fn send(msg: &PluginMessage) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(to_line(msg).as_bytes());
    let _ = out.flush();
}
