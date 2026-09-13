//! **Compliance evidence, as a plugin** (spec 13 / spec 25).
//!
//! The protocol loop. One panel, four commands, and no capabilities at all — the audit
//! reads the workspace itself and writes an HTML site; it asks the editor for nothing but
//! the project path, which arrives in the handshake.
//!
//! # Why this is a plugin
//!
//! The evidence engine is model-free by construction: `sc-comply`'s tree cannot reach a
//! model, which is why the same workspace always yields the same control results. The
//! *prose* is the other half — a model, when chosen, writes exactly two things, the
//! executive summary and the auditor guidance for controls a code scan could not settle,
//! and neither can change a control's status.
//!
//! Keeping that in the editor would have meant the editor keeping a path to a model,
//! which is the one thing it must not have (spec 21). The alternative was deleting the
//! prose. So compliance became a plugin instead, and nobody lost a feature to win a
//! dependency argument.

use std::io::BufRead;
use std::sync::mpsc;

use sc_plugin_proto::manifest::{CommandDecl, Subscription};
use sc_plugin_proto::{
    to_line, HostMessage, Manifest, Outgoing, PanelDecl, PluginMessage, PROTOCOL_VERSION,
};

use sc_plugin_comply::comply::{ComplyModel, ComplyReport};
use sc_plugin_comply::{runner, ui};

/// The panel this plugin contributes.
const PANEL: &str = "compliance";

/// What the plugin is doing right now.
struct State {
    /// Where the project is. `None` until the handshake, and again if it closes.
    workspace: Option<std::path::PathBuf>,
    /// Who writes the summary. Deterministic by default, and never a failure.
    model: ComplyModel,
    /// The audit in flight, if any. `Some` is what "running" means.
    audit: Option<runner::Audit>,
    /// The last outcome, kept so the panel still shows it after the run ends.
    result: Option<Result<ComplyReport, String>>,
}

fn main() {
    let stdin = std::io::stdin();
    let (tx, rx) = mpsc::channel::<runner::Event>();

    let mut state = State {
        workspace: None,
        model: ComplyModel::default(),
        audit: None,
        result: None,
    };

    // The host writes one message per line and waits for nothing, so a blocking read is
    // correct: this loop IS the plugin. The audit runs on its own thread and lands on
    // `rx`, drained after each host message.
    for line in stdin.lock().lines().map_while(Result::ok) {
        if let Outgoing::Message(msg) = sc_plugin_proto::parse_host_line(&line) {
            if handle(&mut state, *msg, &tx) {
                break;
            }
        }
        drain(&mut state, &rx);
    }
}

/// Handle one host message. Returns true to stop.
fn handle(state: &mut State, msg: HostMessage, tx: &mpsc::Sender<runner::Event>) -> bool {
    match msg {
        HostMessage::Initialize { workspace, .. } => {
            state.workspace = workspace.map(std::path::PathBuf::from);
            send(&PluginMessage::Initialized {
                manifest: manifest(),
            });
            // The panel gets content immediately. One that stays blank until the first
            // event is indistinguishable from one that is broken.
            push(state);
        }

        HostMessage::Shutdown => return true,

        HostMessage::WorkspaceChanged { workspace } => {
            state.workspace = workspace.map(std::path::PathBuf::from);
            // A report belongs to the project it was generated from. Keeping the totals
            // on screen after a project switch would be showing one project's evidence
            // under another's name.
            state.result = None;
            push(state);
        }

        // The Run button. The form carries no fields — the model choice is made by the
        // picker rows, so submitting means exactly one thing.
        HostMessage::PanelEvent { panel, .. } if panel == PANEL => {
            start(state, tx);
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
        "comply.run" => start(state, tx),

        "comply.set-model" => {
            // Ignored mid-run: the audit in flight cannot change model, and the panel
            // already renders the rows inert. This is the same rule enforced on the
            // other side of the wire, because a stale panel must not be able to.
            if state.audit.is_some() {
                return;
            }
            let Some(label) = args.first() else { return };
            let Some(m) = ComplyModel::ALL.iter().find(|m| m.label() == label) else {
                return;
            };
            state.model = *m;
            push(state);
        }

        "comply.open-report" => {
            if let Some(Ok(r)) = &state.result {
                open_path(&r.index);
            }
        }

        "comply.clear" => {
            state.result = None;
            push(state);
        }

        _ => {}
    }
}

/// Start an audit, if there is somewhere to run it and nothing already running.
fn start(state: &mut State, tx: &mpsc::Sender<runner::Event>) {
    if state.audit.is_some() {
        return;
    }
    let Some(ws) = state.workspace.clone() else {
        // The panel already says this when no project is open, so reaching here means
        // the command palette. Answer in the same place the result would appear.
        state.result = Some(Err(
            "open a project folder first — the audit reads the workspace".to_string(),
        ));
        push(state);
        return;
    };
    // The previous outcome goes now rather than when the new one lands: leaving it up
    // beside "Auditing…" reads as the result of the run in flight.
    state.result = None;
    state.audit = Some(runner::Audit::start(ws, state.model, tx.clone()));
    push(state);
}

/// Take whatever the worker has produced.
fn drain(state: &mut State, rx: &mpsc::Receiver<runner::Event>) {
    let mut changed = false;
    while let Ok(ev) = rx.try_recv() {
        changed = true;
        match ev {
            runner::Event::Done(result) => {
                state.audit = None;
                state.result = Some(*result);
            }
        }
    }
    if changed {
        push(state);
    }
}

/// Push the panel.
fn push(state: &State) {
    send(&PluginMessage::PanelContent {
        panel: PANEL.to_string(),
        content: ui::panel(&ui::View {
            workspace: state.workspace.as_deref(),
            model: state.model,
            running: state.audit.is_some(),
            result: state.result.as_ref(),
        }),
        scroll: None,
    });
}

/// What this plugin contributes.
fn manifest() -> Manifest {
    Manifest {
        id: "compliance".to_string(),
        name: "Compliance".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        panels: vec![PanelDecl {
            id: PANEL.to_string(),
            title: "Compliance".to_string(),
        }],
        commands: vec![
            cmd("comply.run", "Compliance: generate report"),
            cmd("comply.set-model", "Compliance: choose summary model"),
            cmd("comply.open-report", "Compliance: open the report"),
            cmd("comply.clear", "Compliance: clear the result"),
        ],
        // NONE. The audit reads the workspace itself and writes its own HTML; the only
        // thing it needs from the editor is the project path, and that arrives in the
        // handshake rather than through a capability.
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
///
/// Best-effort: if the pipe is gone the host has closed, and the next read returns EOF
/// and ends the loop. There is nothing useful to do with the error in between.
fn send(msg: &PluginMessage) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(to_line(msg).as_bytes());
    let _ = out.flush();
}

/// Open the finished report in whatever the OS uses for HTML.
///
/// Best-effort and deliberately silent on failure: the panel already shows the path, so a
/// user whose machine has no handler can still find the file. Windowless, because on
/// Windows a plain spawn flashes a console.
fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c.creation_flags(CREATE_NO_WINDOW);
        let _ = c.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
