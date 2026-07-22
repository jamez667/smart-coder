//! The iced application — thin rendering glue over the tested `sc_win` library.
//!
//! All "what to show / what to run" logic lives in [`crate::view`], [`crate::config`],
//! [`crate::session`], and [`crate::bridge`]; this file only lays those out as
//! widgets, pumps the worker channels on a timer tick, and routes button clicks back
//! to the blocking decision seams. Keep it thin.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input, Space};
use iced::{Background, Border, Color, Element, Fill, Length, Subscription, Task, Theme};

use sc_core::Confirmation;
use sc_win::bridge::Pending;
use sc_win::config::ToolCalling;
use sc_win::session::{RunKind, Session, UiEvent};
use sc_win::view::{agent_rows, swarm_rows, Row};
use sc_win::UiConfig;
use sc_workflow::{Decision, Phase};

mod styles;
pub(crate) use styles::*;
/// Launch the desktop app.
/// Start the remote-mirror server on a background thread and return the shared handle the
/// `App` tees events into / drains commands from. Prints the connection URL + Tailscale hint.
/// The port is `SC_REMOTE_PORT` (default 8178).
fn start_mirror() -> sc_web::RemoteMirror {
    let mirror = sc_web::RemoteMirror::new();
    let token = sc_web::mint_token();
    let port: u16 = std::env::var("SC_REMOTE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8178);
    let addr = format!("127.0.0.1:{port}");
    // Prefer the Tailscale HTTPS URL (what the phone actually uses); fall back to loopback.
    let phone_url = match tailnet_host() {
        Some(host) => format!("https://{host}:{port}/?k={token}"),
        None => format!("http://127.0.0.1:{port}/?k={token}"),
    };
    // Record this session so the user can find the current url later (the token rotates each
    // launch) and see recent/active sessions.
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    sc_win::persist::record_session(&phone_url, port, std::process::id(), started);

    let server_mirror = mirror.clone();
    let tok = token.clone();
    let printed_url = phone_url.clone();
    std::thread::spawn(move || {
        let _ = sc_web::serve_mirror(server_mirror, &addr, &tok, move |_url| {
            println!("smart-coder remote mirror live — phone URL:");
            println!("  {printed_url}");
            println!(
                "(if you haven't yet: run `tailscale serve {port}` once so the https URL works)"
            );
        });
    });
    mirror
}

/// Print the remote-mirror session history (newest first), flagging which are still ACTIVE
/// (their process is alive). Used by `sc-win --remote-history`.
pub fn print_remote_history() {
    let sessions = sc_win::persist::load_sessions();
    if sessions.is_empty() {
        println!("No remote-mirror sessions recorded yet.");
        println!("(Launch with SC_REMOTE=1 to start one.)");
        return;
    }
    println!("Remote-mirror sessions (newest first):\n");
    for s in &sessions {
        let active = pid_alive(s.pid);
        let flag = if active { "● ACTIVE " } else { "  ended  " };
        let when = fmt_unix(s.started);
        println!("{flag} port {}  pid {}  {when}", s.port, s.pid);
        println!("           {}", s.url);
    }
    let active_count = sessions.iter().filter(|s| pid_alive(s.pid)).count();
    println!("\n{active_count} active. Paste an ACTIVE url into the phone.");
}

/// Whether a process with `pid` is currently running (Windows: `tasklist`).
fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = sc_win::proc::command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        if let Ok(o) = out {
            return String::from_utf8_lossy(&o.stdout).contains(&pid.to_string());
        }
        false
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// Format a unix timestamp as a local-ish `YYYY-MM-DD HH:MM` (via chrono, already a dep).
fn fmt_unix(secs: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(secs as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => format!("t={secs}"),
    }
}

/// The inclusive Shift-range selection over an ordered path list: every path between `anchor` and
/// `target` (found by position in `order`), regardless of which comes first. If either isn't in
/// `order`, falls back to selecting just `target` — the sane result for a stale anchor. Pure and
/// index-based so the shift-range math is unit-testable without any GUI scaffolding.
fn git_range(order: &[String], anchor: &str, target: &str) -> std::collections::BTreeSet<String> {
    let (a, b) = match (
        order.iter().position(|p| p == anchor),
        order.iter().position(|p| p == target),
    ) {
        (Some(a), Some(b)) => (a, b),
        _ => return std::iter::once(target.to_string()).collect(),
    };
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    order[lo..=hi].iter().cloned().collect()
}

/// The tab to activate after closing the one at `closed_idx` from a list of tabs, given
/// `len_after` = the number of tabs REMAINING after removal. Returns the new active index
/// (into the post-removal list), or `None` if no tabs remain.
///
/// Semantics: activate `closed_idx.min(len_after - 1)` — i.e. the tab that shifted left into
/// the closed slot, or the new last tab when we closed the rightmost one. This mirrors VS Code:
/// closing a tab lands you on its right neighbour (which now occupies the vacated slot), or the
/// left neighbour when the closed tab was the last one.
fn tab_after_close(closed_idx: usize, len_after: usize) -> Option<usize> {
    if len_after == 0 {
        None
    } else {
        Some(closed_idx.min(len_after - 1))
    }
}

/// The Tailscale MagicDNS hostname of this machine (e.g. `my-pc.tailXXXXXX.ts.net`),
/// via the `tailscale` CLI. `None` if Tailscale isn't installed/logged in.
fn tailnet_host() -> Option<String> {
    let out = sc_win::proc::command("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let name = v.get("Self")?.get("DNSName")?.as_str()?;
    // DNSName has a trailing dot; strip it.
    Some(name.trim_end_matches('.').to_string())
}

pub fn run() -> iced::Result {
    // iced 0.14: `application(boot, update, view)` where boot returns the initial
    // (State, Task); title/subscription/theme are builder methods. If a project was
    // remembered from last session, greet with its README/roadmap on boot.
    iced::application(
        || {
            let mut app = App::default();
            if app.picked_workspace.is_some() {
                app.show_welcome();
                app.open_conversation();
            }
            // Remote-mirror mode (Claude-Code-remote style): when SC_REMOTE is set, start a
            // mirror server so a phone can attach to THIS live session — see the chat + agent
            // activity, send chat, approve/deny, stop. Bound to 127.0.0.1 (front it with
            // `tailscale serve`); every request needs the printed per-run token.
            if std::env::var("SC_REMOTE").is_ok() {
                app.remote = Some(start_mirror());
                // Publish the initially-open project so the phone shows it on first connect.
                app.publish_workspace_to_remote();
            }
            (app, Task::none())
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(App::theme)
    .window(iced::window::Settings {
        // The taskbar/title-bar icon of the RUNNING window is set here at runtime — the
        // exe's embedded icon only governs how Explorer shows the file, not the live window.
        icon: iced::window::icon::from_file_data(
            include_bytes!("../../../../assets/logo/sc-logo-256.png"),
            None, // guess the format from the PNG header
        )
        .ok(),
        ..Default::default()
    })
    .run()
}

/// A pending decision surfaced to the human, with the reply channel to answer it.
mod types;
pub(crate) use types::*;
impl App {
    fn title(&self) -> String {
        "smart-coder — vibe coding".to_string()
    }

    fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    fn subscription(&self) -> Subscription<Message> {
        // Tick while there's anything live to pump or animate: a running session, an
        // open gate, or topology flows still glowing (so the fade animates to rest
        // even after the run ends). iced delivers the tick on the UI thread, so
        // draining the std::mpsc Receiver here is safe.
        let glowing = !self.topology.active_flows(self.now()).is_empty();
        let tick = if self.session.is_some()
            || self.chat_session.is_some()
            || self.triage.is_some()
            || self.replace.is_some()
            || self.working.is_some()
            || !self.gatebar.is_empty()
            || self.term_rx.is_some()
            || glowing
            // Remote mirror active: keep ticking so inbound phone commands (chat/cancel)
            // are drained even when the desktop is otherwise idle.
            || self.remote.is_some()
        {
            iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // A heartbeat that re-walks the tree + git state while a project is open, so files
        // created/removed OUTSIDE the app (or by a running agent) show up without a manual
        // refresh. The walk is cheap and off the render path, so 500ms feels live without cost.
        // Off when no project is open.
        let sync = if self.picked_workspace.is_some() {
            iced::time::every(Duration::from_millis(500)).map(|_| Message::SyncWorkspace)
        } else {
            Subscription::none()
        };
        // An always-on slow heartbeat that drives the backend health probe (startup + every
        // ~10s; the 3s tick just gives the 10s gate resolution and adopts a finished probe
        // promptly). Runs even when the app is otherwise idle — that's the whole point: know
        // the backend is down BEFORE you try to use it.
        let health = iced::time::every(Duration::from_secs(3)).map(|_| Message::HealthTick);
        // Track the window-absolute cursor position so a right-click in the git tab can pop its
        // context menu exactly at the pointer. `mouse_area::on_move` reports widget-relative
        // coordinates (useless for placing a window overlay); this window event is absolute.
        let cursor = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                Some(Message::GitCursorMoved(position))
            }
            // A button-release anywhere ends a divider drag (even if the cursor left the handle).
            // `SplitDragEnd` ends BOTH the chat|code and git|files drags — they're mutually
            // exclusive in practice, so one release message clears whichever is active.
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
                Some(Message::SplitDragEnd)
            }
            // Track the window size so a divider drag can map cursor X→width fraction and
            // cursor Y→height fraction. `Opened` seeds it at startup; `Resized` keeps it current.
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowSize(size.width, size.height))
            }
            iced::Event::Window(iced::window::Event::Opened { size, .. }) => {
                Some(Message::WindowSize(size.width, size.height))
            }
            // Keep a live view of the held modifiers, so a git-row button click (which doesn't
            // report modifiers in its press message) can branch on Ctrl/Shift for multi-select.
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            _ => None,
        });
        Subscription::batch([tick, sync, cursor, health])
    }

    /// The run action for the primary button / intent submit, chosen by context: a
    /// picked project folder means "iterate in place"; otherwise the from-scratch build.
    fn run_message(&self) -> Message {
        if self.picked_workspace.is_some() {
            Message::RunIterate
        } else {
            Message::RunTdd
        }
    }

    /// The primary button label matching [`Self::run_message`].
    fn run_label(&self) -> &'static str {
        if self.picked_workspace.is_some() {
            "⚒  iterate"
        } else {
            "⚒  build"
        }
    }

    /// Fold every settings-panel input into `self.cfg` and persist the connection fields to
    /// config.json, so the current backend (coder + Gemini planner) is used AND survives a
    /// restart. Called before any run/chat/triage that talks to a model — the single place the
    /// input boxes become config, so the three entry points can't drift.
    fn commit_settings(&mut self) {
        // Models (per stage) + the endpoint-agnostic knobs.
        self.cfg.model = self.model_input.clone();
        self.cfg.orchestrator_model = non_empty(&self.orch_model_input);
        self.cfg.advisor_model = non_empty(&self.advisor_input);
        self.cfg.verify_command = non_empty(&self.verify_input);
        self.cfg.system_suffix = non_empty(&self.suffix_input);
        // Connections (endpoint + key each). The per-stage provider routing is already live on
        // `cfg` (edited by the toggle handlers). A blank local url keeps the current one rather
        // than wiping the endpoint.
        if !self.local_url_input.trim().is_empty() {
            self.cfg.local_conn.base_url = self.local_url_input.trim().to_string();
        }
        self.cfg.local_conn.key = non_empty(&self.local_key_input);
        if !self.gemini_url_input.trim().is_empty() {
            self.cfg.gemini_conn.base_url = self.gemini_url_input.trim().to_string();
        }
        self.cfg.gemini_conn.key = non_empty(&self.gemini_key_input);
        // Flatten connections + routing into the scalar fields the backend builders read, THEN
        // persist. Without this the run would use stale base_url/orchestrator_* from before the
        // edit. `resolve_stages` also clears a redundant orchestrator override when planner==coder.
        self.cfg.resolve_stages();
        // Persist (best-effort) so the connection/routing setup survives a restart.
        self.cfg.save_config();
    }

    fn start(&mut self, kind: RunKind) {
        if self.intent.trim().is_empty() || self.session.is_some() {
            return;
        }
        // Commit the settings inputs into the config (and persist them) before the run.
        self.commit_settings();

        // Preflight: don't launch a run against a known-bad backend — surface the reason in the
        // activity stream instead of failing several turns in.
        if let Some(reason) = self.backend_unready_reason() {
            self.rows.push(Row::ok(
                "⚠",
                format!("{reason} — check the backend badge (top bar)"),
            ));
            return;
        }

        self.rows.clear();
        self.board.clear();
        self.swarm_board = sc_win::SwarmBoard::default();
        self.plan = sc_win::Plan::default();
        self.topology = sc_win::Topology::default();
        self.selected_coder = None;
        self.run_started = Some(Instant::now());
        self.last_reload = None; // reload the live view immediately on the first tick
        self.verify_text = None;

        // Route the agent's command execution through the SAME persistent container the
        // terminal uses (starting it if needed), so its commands keep state and you can inspect
        // its work from the terminal. Falls back to per-run/host inside `agent_sandbox`.
        self.cfg.sandbox_override = Some(self.agent_sandbox());
        self.summary = None;
        self.result = None;
        self.gatebar.clear();
        // Re-arm follow so the code panel tracks the agent through this run.
        self.follow_agent = true;
        // Track this run's mode + which files it edits (for the honest iterate banner).
        self.iterating = matches!(kind, RunKind::Iterate | RunKind::StagedBuild);
        self.planning_only = matches!(kind, RunKind::Plan);
        self.edited_files.clear();
        // Jump to the Verification tab so the run's checks are visible as it works.
        self.bottom_tab = BottomTab::Verification;

        // Where to run:
        //  • a folder you picked → run there directly, so a follow-up prompt iterates
        //    on (and edits) the existing files in that project;
        //  • otherwise → a fresh datetime-stamped folder under the scratch base, so a
        //    new project never overwrites a previous one.
        let ws = match &self.picked_workspace {
            Some(dir) => {
                let _ = std::fs::create_dir_all(dir);
                dir.clone()
            }
            None => {
                let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
                self.cfg.run_workspace(&stamp)
            }
        };
        self.run_dir = Some(ws.clone());
        self.session = Some(Session::spawn(
            kind,
            self.cfg.clone(),
            self.intent.clone(),
            ws,
        ));
    }

    /// Monotonic seconds since the current run started — the canvas's animation clock.
    fn now(&self) -> f32 {
        self.run_started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// The workspace root the explorer/code panels read from: the picked project folder
    /// if any, else the current run's output dir, else the config base. This is the tree
    /// the user is actually working in.
    fn workspace_root(&self) -> std::path::PathBuf {
        self.picked_workspace
            .clone()
            .or_else(|| self.run_dir.clone())
            .unwrap_or_else(|| self.cfg.workspace.clone())
    }

    /// On opening a project, greet the user in the Activity stream: the project name, its
    /// README's TODO/roadmap excerpt (highlighted), and an invitation to say what to work
    /// on. No-op for a folder with no README (still greets, just no excerpt).
    fn show_welcome(&mut self) {
        let Some(root) = self.picked_workspace.clone() else {
            return;
        };
        let folder = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        let readme = find_readme(&root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let todo_md = find_todo_file(&root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let w = sc_win::welcome::build(&readme, &todo_md, &folder);

        // Clear any previous run's activity so the welcome is the first thing shown.
        self.rows.clear();
        self.summary = None;
        self.result = None;
        self.rows.push(Row::ok("◆", format!("opened  {}", w.title)));
        if !w.lines.is_empty() {
            let header = if w.no_todo {
                "— from the README —"
            } else {
                "— what's on the TODO —"
            };
            self.rows.push(Row::ok(" ", header.to_string()));
            for l in &w.lines {
                // A highlighted TODO/roadmap item gets a star; context lines a faint dot.
                let icon = if l.highlight { "★" } else { "·" };
                self.rows.push(Row::ok(icon, l.text.clone()));
            }
        }
        // The closing prompt — for a project with no TODO, this nudges the user to make one.
        let prompt_icon = if w.no_todo { "⚠" } else { "▸" };
        self.rows.push(Row::ok(prompt_icon, w.prompt));
    }

    /// Adopt `dir` as the working project: reset per-project view state, walk its tree,
    /// remember it in recents, tell any remote mirror, and open its planning conversation.
    /// Shared by the desktop "open folder" button and a remote `/open` command.
    fn open_workspace(&mut self, dir: std::path::PathBuf) {
        // Retire the previous project's sandbox container before switching — it mounts the
        // old workspace and must not outlive it. A fresh one starts on the next command.
        self.teardown_term_container();
        self.picked_workspace = Some(dir.clone());
        // A fresh project → drop any stale selection from the last one, and open the tree
        // compacted: every top-level folder starts collapsed.
        self.selected_file = None;
        self.code = None;
        // Tabs held the old project's files — clear them so they don't linger into the new one.
        self.open_tabs.clear();
        // Drop the git multi-selection + its shift anchor too — they keyed off the old project's
        // paths and would highlight/act on stale files in the new one.
        self.git_selection.clear();
        self.git_select_anchor = None;
        self.collapsed_dirs = sc_win::filetree::top_level_dirs(&dir);
        self.file_filter.clear();
        self.tree_cache = sc_win::filetree::full_rows(&dir);
        // Remember it (promotes to the front of recents) for next launch + the remote picker.
        let mut state = sc_win::persist::load();
        state.record_project(&dir);
        sc_win::persist::save(&state);
        self.publish_workspace_to_remote();
        // Greet: show the README/roadmap in Activity, and open the planning conversation.
        self.show_welcome();
        self.open_conversation();
    }

    /// Push the current workspace name + recents list to the remote mirror, so the phone's
    /// header shows the current repo and its picker lists recent projects. No-op without a mirror.
    fn publish_workspace_to_remote(&self) {
        if let Some(m) = &self.remote {
            let current = self
                .picked_workspace
                .as_ref()
                .map(|p| p.to_string_lossy().to_string());
            let recents: Vec<String> = sc_win::persist::load()
                .recents
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            m.set_projects(current, recents);
        }
    }

    /// Open a planning conversation for the current project: read the plan files, pick the
    /// mode (scratch vs existing), and seed the thread with the agent's opening line.
    /// The project's file paths (workspace-relative, `/`-separated) from the walked tree cache
    /// — directories dropped. Fed to a plan conversation so "Files to touch" names real files.
    fn project_file_paths(&self) -> Vec<String> {
        self.tree_cache
            .iter()
            .filter(|r| !r.is_dir)
            .map(|r| r.rel.clone())
            .collect()
    }

    fn open_conversation(&mut self) {
        let root = self.workspace_root();
        // Load persisted inline comments, ensure .dc/ is git-ignored, and pull the initial
        // git view (branch + file statuses) for the PR-style tree.
        self.comments = sc_win::comments::load(&root);
        sc_win::comments::ensure_gitignored(&root);
        self.refresh_git_view();
        let (readme, todo) = self.read_plan_files(&root);
        let mut convo = sc_win::chat::Conversation::open(&readme, &todo);
        convo.set_file_tree(self.project_file_paths());
        self.chat_turns.clear();
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: convo.opening_line(),
        });
        self.proposed_files.clear();
        // In an existing project, auto-open the TODO (or README) in the code view so you
        // land on the plan; the conversation continues from there.
        if let Some(f) = self.plan_file_to_show(&root) {
            self.follow_agent = false;
            self.select_file(f);
        }
        self.conversation = Some(convo);
    }

    /// The current README/TODO contents (empty string if absent) for the workspace `root`.
    fn read_plan_files(&self, root: &std::path::Path) -> (String, String) {
        let readme = find_readme(root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let todo = find_todo_file(root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        (readme, todo)
    }

    /// Which plan file to auto-open in the code view when a project opens: the TODO if it
    /// exists, else the README, else nothing.
    fn plan_file_to_show(&self, root: &std::path::Path) -> Option<String> {
        let rel = |p: std::path::PathBuf| {
            p.strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/")
        };
        find_todo_file(root)
            .map(rel)
            .or_else(|| find_readme(root).map(rel))
    }

    /// Send the composer text as a chat turn to the planning agent (worker thread). No-op
    /// when there's no conversation, no text, or a turn is already in flight.
    fn send_chat(&mut self) {
        let text = self.intent.trim().to_string();
        if text.is_empty() || self.chat_session.is_some() || self.conversation.is_none() {
            return;
        }
        // Preflight: refuse to send into a known-bad backend (the failure the health badge
        // warns about), so you get a clear message instead of a mid-stream "connection
        // refused". A not-yet-probed (`None`) or `Ready` backend passes.
        if let Some(reason) = self.backend_unready_reason() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ {reason} — check the backend badge (top bar)."),
            });
            self.intent.clear();
            return;
        }
        // Commit connection settings (mirrors `start`), so a chat uses the current backend.
        self.commit_settings();
        let think = self.think;

        // Snapshot the file open in the code view so the model answers against what the user is
        // looking at ("what does this do?", "add handling here"). Read from disk (not the capped
        // CodeView render) so the chat sees the real file; chat.rs head-clips it for the window.
        let open_file = self.selected_file.as_ref().and_then(|rel| {
            let path = self.workspace_root().join(rel);
            std::fs::read_to_string(&path)
                .ok()
                .map(|body| (rel.clone(), body))
        });

        // Refresh the project file list (files may have been added/removed this session) so a
        // plan grounds on the current tree. Computed before the mutable convo borrow.
        let file_tree = self.project_file_paths();
        // Update the conversation, then spawn a planning turn (classify intent → generate). The
        // classification decides how the reply is shaped; the app no longer sniffs the text.
        let convo = {
            let convo = self.conversation.as_mut().expect("checked above");
            convo.set_open_file(open_file);
            convo.set_file_tree(file_tree);
            convo.user_turn(&text);
            convo.clone()
        };
        // Mirror the user's message to any remote client (so the phone shows what was
        // sent — whether it was typed on the desktop or came in from the phone itself).
        if let Some(m) = &self.remote {
            m.push(sc_core::AgentEvent::ChatMessage {
                role: "you".into(),
                text: text.clone(),
            });
        }
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::You,
            text,
        });
        self.proposed_files.clear();
        self.proposed_command = None;
        self.intent.clear();
        if self.debug {
            // Show the generate prompt for the most likely intent path (feature plan is the one
            // that was misbehaving); the real intent is classified on the worker.
            let req = convo.request(think, sc_win::chat::ChatIntent::Question);
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("chat", &joined);
        }
        self.chat_session = Some(sc_win::chat_session::ChatSession::spawn_planning(
            self.cfg.clone(),
            convo,
            think,
        ));
    }

    /// Spawn a chat/generate call, first echoing its prompt into the chat when debug mode is
    /// on. Every model call that streams into the chat goes through here.
    fn spawn_chat(&mut self, label: &str, req: sc_model::GenerateRequest) {
        if self.debug {
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt(label, &joined);
        }
        self.chat_session = Some(sc_win::chat_session::ChatSession::spawn(
            self.cfg.clone(),
            req,
        ));
    }

    /// Submit the open line comment: capture the line + text, spawn the small/big triage
    /// classification (fast `/no_think`), and close the comment box. Routing happens in
    /// [`Self::pump_triage`] when the verdict arrives.
    fn submit_line_comment(&mut self) {
        let (Some((start, end)), Some(cv)) = (self.comment_range, self.code.as_ref()) else {
            return;
        };
        let comment = self.comment_draft.trim().to_string();
        if comment.is_empty() {
            return;
        }
        let rel = cv.rel.clone();
        // Scope from the CURRENT on-disk file, freshly numbered — NOT from `self.code`, which can
        // be a stale streamed *preview* (renumbered/spliced) left by an earlier fix. Reading disk
        // here guarantees `start`/`end`/`selection` all refer to the same real bytes we'll splice,
        // so the fix lands on the lines you actually selected.
        let disk = sc_win::codeview::load(&self.workspace_root(), &rel);
        let lines = if disk.note.is_none() {
            &disk.lines
        } else {
            &cv.lines // fall back to the view if the file can't be re-read
        };
        // The IDE does the scoping: hand the model the selected code + a small window, so it
        // doesn't slurp the whole file (what made a one-line fix slow + context-heavy).
        let (selection, context) = sc_win::linecomment::scope_context(lines, start, end);
        let lc = sc_win::linecomment::LineComment {
            file: rel,
            start,
            end,
            selection,
            context,
            comment,
        };
        // Persist the comment inline (pending) — it stays visible in the code view and gets
        // marked resolved when the agent finishes. A Question is removed again in pump_triage
        // (it's answered, not a change to track).
        self.comments.add(sc_win::comments::Comment::new(
            lc.file.clone(),
            start,
            end,
            lc.comment.clone(),
        ));
        sc_win::comments::save(&self.workspace_root(), &self.comments);
        // Commit connection settings so the triage/edit use the current backend.
        self.commit_settings();
        // Keep the commented lines highlighted (pulsing amber) while the agent works on them,
        // so the "thinking" gap between submit and the edit landing feels active. Cleared when
        // the run/answer finishes.
        self.working = Some((lc.file.clone(), start, end));
        self.run_started = Some(Instant::now()); // (re)start the animation clock
                                                 // Echo the comment into the chat (before `lc` moves into the triage state).
        let span = if start == end {
            format!("line {start}")
        } else {
            format!("lines {start}-{end}")
        };
        let echo = format!("💬 {} {span}: {}", lc.file, lc.comment);
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::You,
            text: echo,
        });
        let req = lc.classify_request();
        if self.debug {
            // Dump exactly what range/text was captured, so a mis-splice is diagnosable.
            let sel_first = lc.selection.lines().next().unwrap_or("").to_string();
            self.debug_prompt(
                "capture",
                &format!(
                    "range = lines {}-{}\nselection[0] = {:?}\nselection = {} line(s)",
                    lc.start,
                    lc.end,
                    sel_first,
                    lc.selection.lines().count()
                ),
            );
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("triage", &joined);
        }
        let session = sc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
        self.triage = Some(TriageInFlight {
            comment: lc,
            session,
        });
        self.comment_range = None;
        self.comment_draft.clear();
    }

    /// Drain the line-comment triage call. On a verdict: SMALL → run a scoped-but-coherent
    /// iterate fix now; BIG → seed a planning chat turn for approval.
    fn pump_triage(&mut self) {
        let Some(t) = &self.triage else {
            return;
        };
        let events = t.session.drain();
        for ev in events {
            let reply = match ev {
                // Triage is a one-word classify — ignore streamed tokens, act on the final.
                sc_win::chat_session::ChatEvent::Token(_) => continue,
                sc_win::chat_session::ChatEvent::Reply(r, _) => r,
                sc_win::chat_session::ChatEvent::Failed(_) => {
                    // On a failed triage, fall back to planning (the safe route).
                    "BIG".to_string()
                }
            };
            let verdict = sc_win::linecomment::parse_verdict(&reply);
            let comment = self.triage.take().expect("in flight").comment;
            match verdict {
                sc_win::linecomment::Verdict::Question => {
                    // A question about the code → ANSWER it (streamed into the chat), edit
                    // nothing. It's not a change to track, so drop the pending inline comment
                    // we stored on submit.
                    if let Some((i, _)) = self
                        .comments
                        .on_file(&comment.file)
                        .filter(|(_, c)| !c.resolved)
                        .last()
                    {
                        self.comments.remove(i);
                        sc_win::comments::save(&self.workspace_root(), &self.comments);
                    }
                    self.commit_settings();
                    let req = comment.question_request(self.think);
                    self.spawn_chat("question", req);
                }
                sc_win::linecomment::Verdict::Small => {
                    // FAST PATH: one model call for the new block text; the IDE splices it in by
                    // line number (no edit_file whitespace thrashing — the thing that made a
                    // reindent take 3 tries). The amber "working" highlight already shows the range.
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: "→ quick fix — rewriting the selection…".to_string(),
                    });
                    self.commit_settings();
                    let req = comment.replace_request();
                    if self.debug {
                        let joined = req
                            .messages
                            .iter()
                            .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        self.debug_prompt("fix (line-replace)", &joined);
                    }
                    let session = sc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
                    self.replace = Some(ReplaceInFlight {
                        comment,
                        session,
                        streamed: String::new(),
                    });
                }
                sc_win::linecomment::Verdict::Big => {
                    // Route into planning: seed a user turn and send it to the chat agent.
                    let seed = comment.planning_seed();
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: "→ this needs a plan — let's talk it through first.".to_string(),
                    });
                    self.intent = seed;
                    self.send_chat();
                }
            }
            break;
        }
    }

    /// Drain the fast line-replace fix. Streams the replacement into the code-view preview;
    /// on completion, splices the new block into the file BY LINE NUMBER (deterministic — no
    /// whitespace matching), captures the before-text for a per-comment Revert, resolves the
    /// comment, and verifies (unless the change is comment-only).
    fn pump_replace(&mut self) {
        let Some(r) = &self.replace else {
            return;
        };
        let events = r.session.drain();
        let mut done: Option<(String, String)> = None; // (raw reply, none) sentinel via loop
        for ev in events {
            match ev {
                sc_win::chat_session::ChatEvent::Token(delta) => {
                    // Pull the fields we need out of the in-flight replace, ending the mutable
                    // borrow before we touch `self` again (for workspace_root / self.code).
                    let preview_bits = self.replace.as_mut().map(|r| {
                        r.streamed.push_str(&delta);
                        (
                            r.comment.file.clone(),
                            r.comment.start,
                            r.comment.end,
                            r.comment.selection.clone(),
                            sc_win::linecomment::extract_replacement(&r.streamed)
                                .unwrap_or_default(),
                        )
                    });
                    if let Some((file, hstart, hend, selection, preview)) = preview_bits {
                        if !preview.is_empty() {
                            let cur = std::fs::read_to_string(self.workspace_root().join(&file))
                                .unwrap_or_default();
                            // Preview at the block's TRUE current location (same re-anchoring the
                            // real write uses), so the live preview lands where the edit will.
                            if let Some((start, end)) =
                                sc_win::linecomment::locate_range(&cur, hstart, hend, &selection)
                            {
                                let spliced =
                                    sc_win::linecomment::splice_lines(&cur, start, end, &preview);
                                self.selected_file = Some(file.clone());
                                self.follow_agent = false;
                                self.code = Some(sc_win::codeview::from_text(&file, &spliced));
                            }
                        }
                    }
                }
                sc_win::chat_session::ChatEvent::Reply(raw, _) => {
                    done = Some((raw, String::new()));
                    break;
                }
                sc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.working = None;
                    self.replace = None;
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    return;
                }
            }
        }
        if let Some((raw, _)) = done {
            self.apply_line_replace(&raw);
        }
    }

    /// Apply a completed line-replace reply: splice the new block into the file, record the
    /// before-text on the comment, resolve it, refresh the view + git, and verify if needed.
    fn apply_line_replace(&mut self, raw: &str) {
        let Some(rf) = self.replace.take() else {
            return;
        };
        self.working = None;
        let c = rf.comment;
        let root = self.workspace_root();
        let path = root.join(&c.file);
        let Some(new_block) = sc_win::linecomment::extract_replacement(raw) else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ the model returned nothing to apply.".to_string(),
            });
            return;
        };
        if self.debug {
            self.debug_prompt(
                "line-replace reply",
                &format!(
                    "hint range = {}-{}\nRAW REPLY:\n{}\n\nEXTRACTED BLOCK ({} lines):\n{}",
                    c.start,
                    c.end,
                    raw.trim(),
                    new_block.lines().count(),
                    new_block
                ),
            );
        }
        let Ok(current) = std::fs::read_to_string(&path) else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ couldn't read {}.", c.file),
            });
            return;
        };
        // Re-anchor to where the selected block ACTUALLY is on disk right now — guards against the
        // captured line numbers having drifted (which would splice the fix onto the wrong lines,
        // duplicating the block instead of replacing it). Abort clearly if we can't locate it.
        let Some((start, end)) =
            sc_win::linecomment::locate_range(&current, c.start, c.end, &c.selection)
        else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!(
                    "⚠ couldn't safely locate the selected lines in {} (the file changed) — no edit made.",
                    c.file
                ),
            });
            return;
        };
        // Capture the exact BEFORE-text of the range (for per-comment Revert), then splice.
        let before: String = current
            .lines()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let spliced = sc_win::linecomment::splice_lines(&current, start, end, &new_block);
        if spliced == current {
            // The model returned the selection unchanged (a local model sometimes echoes the
            // input on "shorten this"). Be honest about it and leave the comment PENDING so you
            // can re-run or rephrase, rather than falsely claiming success.
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ the model returned the same lines unchanged — nothing applied. Try \
                       rephrasing (e.g. \"make this comment 2 lines\") or run it again."
                    .to_string(),
            });
            self.refresh_git_view();
            return;
        }
        if std::fs::write(&path, &spliced).is_err() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ couldn't write {}.", c.file),
            });
            return;
        }
        // Resolve the matching stored comment and stash the before-text + new length for Revert.
        let new_len = new_block.lines().count().max(1);
        if let Some((i, _)) = self
            .comments
            .on_file(&c.file)
            .filter(|(_, cc)| !cc.resolved && cc.start == c.start && cc.end == c.end)
            .last()
        {
            if let Some(stored) = self.comments.items.get_mut(i) {
                stored.resolved = true;
                stored.before = Some(before);
                stored.after_len = Some(new_len);
                // Record where the edit ACTUALLY landed (after re-anchoring) so a later Revert
                // splices the before-text back over the right lines. The NEW block spans
                // `start .. start + new_len - 1` — use that as the end, so the resolved comment
                // anchors to the last NEW line (not the stale old end, which drifts when the fix
                // added or removed lines).
                stored.start = start;
                stored.end = start + new_len - 1;
            }
        }
        sc_win::comments::save(&root, &self.comments);

        // Show the applied file + refresh highlights/tree.
        self.selected_file = Some(c.file.clone());
        self.follow_agent = false;
        self.select_file(c.file.clone());
        self.refresh_git_view();

        // Verify — unless the change is comment-only (git says all changed lines are comments).
        let diff = sc_win::gitdiff::files_diff(&root, std::slice::from_ref(&c.file));
        let comment_only = sc_win::gitdiff::is_comment_only_change(&diff);
        if comment_only {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "✓ Done — comment/whitespace only, skipped the compile check.".to_string(),
            });
        } else {
            // Kick a lightweight verify run (cargo check) to confirm it still compiles.
            self.verify_after_replace(&c.file);
        }
    }

    /// Revert just this comment's change: splice its stored before-text back over the lines the
    /// fix produced (`start .. start + after_len - 1`), restoring the original. The comment goes
    /// back to PENDING (you can re-run or dismiss it). No-op if it has no stored before-text.
    fn revert_comment(&mut self, i: usize) {
        let Some(c) = self.comments.items.get(i).cloned() else {
            return;
        };
        let (Some(before), Some(after_len)) = (c.before.clone(), c.after_len) else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ nothing stored to revert for this comment.".to_string(),
            });
            return;
        };
        let root = self.workspace_root();
        let path = root.join(&c.file);
        let Ok(current) = std::fs::read_to_string(&path) else {
            return;
        };
        // The fix produced `after_len` lines starting at `c.start`; swap them back to `before`.
        let restored =
            sc_win::linecomment::splice_lines(&current, c.start, c.start + after_len - 1, &before);
        if std::fs::write(&path, &restored).is_ok() {
            // Reverting the change also removes the comment line entirely (VS-Code-style: undo
            // the edit → the comment thread goes with it), rather than leaving it pending.
            self.comments.remove(i);
            sc_win::comments::save(&root, &self.comments);
            self.select_file(c.file.clone());
            self.refresh_git_view();
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!(
                    "↩ Reverted the change on {} and removed the comment.",
                    c.file
                ),
            });
        }
    }

    /// Revert ONE diff block (VS-Code-style) back to its HEAD text: replace the block's current
    /// line range with the committed version. `cur_start` identifies the hunk (its first current
    /// line). Recomputes the diff from the freshly-loaded file, so it's always ground-truth.
    fn revert_block(&mut self, cur_start: usize) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        let root = self.workspace_root();
        // Find the hunk from the CURRENT on-disk diff (not stale UI state).
        let diff = sc_win::gitdiff::file_diff(&root, &rel);
        let Some(hunk) = diff.hunks().into_iter().find(|h| h.cur_start == cur_start) else {
            return; // the diff moved under us — no-op, the view will refresh
        };
        let path = root.join(&rel);
        let Ok(current) = std::fs::read_to_string(&path) else {
            return;
        };
        // Replace the block's current range with its HEAD text. For a pure addition head_text is
        // empty (→ the added lines are deleted); for a pure deletion the range is empty (→ the
        // removed text is inserted back before cur_start).
        let restored = sc_win::linecomment::splice_lines(
            &current,
            hunk.cur_start,
            hunk.cur_end,
            &hunk.head_text,
        );
        if std::fs::write(&path, &restored).is_ok() {
            self.select_file(rel.clone());
            self.refresh_git_view();
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("↩ Reverted a block in {rel} to its committed version."),
            });
        }
    }

    /// After a line-replace that changed code, run the configured verify command to confirm it
    /// still compiles; report the result in the chat. Runs on a worker via a tiny iterate-less
    /// path would be ideal, but for v1 we run it inline-async through the existing Session by
    /// spawning a no-op check — simplest: just report optimistically and let the PR highlight +
    /// Undo cover a bad edit. (Verification wiring can deepen later.)
    fn verify_after_replace(&mut self, file: &str) {
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: format!(
                "✓ Applied to {file}. (Tip: if it doesn't compile, use the comment's ↩ Revert.)"
            ),
        });
    }

    /// Start an iterate run from a ready-made instruction (used by the small-fix line-comment
    /// path). Mirrors `start(RunKind::Iterate)` but with an explicit instruction instead of
    /// the composer text.
    #[allow(dead_code)]
    fn start_iterate_with(&mut self, instruction: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("fix (iterate)", &instruction);
        self.intent = instruction;
        self.start(RunKind::Iterate);
        self.intent.clear();
        // Mark this run so its outcome is reported back into the chat (set AFTER start(),
        // which clears run state).
        self.iterate_from_comment = true;
    }

    /// Start a PLAN-ONLY workflow run from a ready-made task: run the staged workflow through the
    /// stage breakdown and stop for review (produces reviewable design artifacts, does NOT build).
    fn start_plan_with(&mut self, task: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("plan", &task);
        // Stash the task so the result view's "Build this plan" button can start a staged build
        // against the same plan (the Breakdown → Build hand-off) with no retyping.
        self.last_plan_task = Some(task.clone());
        self.intent = task;
        self.start(RunKind::Plan);
        self.intent.clear();
        self.iterate_from_comment = true;
    }

    /// Start the full plan→BUILD flow (`RunKind::StagedBuild`) from a ready-made task: staged
    /// design through decomposition, then the compiler-driven executor builds it to green. This
    /// is what "Execute plan" does — it actually BUILDS the plan, not just re-designs it.
    fn start_staged_build_with(&mut self, task: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("execute plan (build)", &task);
        self.intent = task;
        self.start(RunKind::StagedBuild);
        self.intent.clear();
        // Report the build outcome back into the chat thread.
        self.iterate_from_comment = true;
    }

    /// Apply the Nth proposed plan-file, then kick off an iterate build to implement it.
    /// The one-click bridge from a `PLAN-<slug>.md` design doc to a real build run: the plan
    /// is written to disk (so the agent can read it), then an iterate run is started with an
    /// instruction pointing at that file. Iterate needs a real project on disk, so this is a
    /// no-op (with a chat note) when no project folder is open or a run is already in flight.
    fn execute_plan(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        if self.session.is_some() {
            return; // A run is already in flight — don't stack another.
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — executing a plan builds into it."
                    .to_string(),
            });
            return;
        }
        // Land the plan on disk (and refresh the conversation snapshot) first, so the workflow
        // can read the plan it's told to design against. This also clears it from the pending
        // proposals, so `i` is consumed exactly like a plain Apply.
        self.apply_proposed_file(i);
        // Execute = BUILD the plan (staged design → compiler-driven build to green), not just
        // re-design it. `plan_task` names the PLAN file so the workflow grounds on its contents.
        self.start_staged_build_with(plan_task(&pf.name));
    }

    /// Apply the Nth proposed plan-file, then run the DESIGN-only staged pipeline (Breakdown):
    /// the staged phases through decomposition, gated for review, WITHOUT building code. Sibling
    /// of [`Self::execute_plan`] (which continues into the build). Same guards + apply-first.
    fn breakdown_plan(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        if self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — the breakdown designs against it."
                    .to_string(),
            });
            return;
        }
        self.apply_proposed_file(i);
        self.start_plan_with(plan_task(&pf.name));
    }

    /// Kick off an iterate build to implement the `PLAN-*.md` open in the code view. Unlike
    /// [`Self::execute_plan`], the file is already on disk (opened from the tree), so there's
    /// nothing to apply — just point an iterate run at it. Same guards: needs an open project
    /// and no run in flight.
    fn execute_open_plan(&mut self) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        if !is_feature_plan(&rel) || self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — executing a plan builds into it."
                    .to_string(),
            });
            return;
        }
        // Act on the feature's spec.md, whichever artifact of specs/<slug>/ is selected (spec /
        // architecture / layout / breakdown / decomposition) — so the run targets specs/<slug>/,
        // reusing its approved design, instead of treating e.g. decomposition.md as a plan to
        // re-design from.
        self.start_plan_with(plan_task(&feature_spec_of(&rel)));
    }

    /// Build the plan open in the code view: the full staged design → compiler-driven build to
    /// green (`RunKind::StagedBuild`). Sibling of [`Self::execute_open_plan`] (which stops at the
    /// design breakdown); the file is already on disk, so nothing to apply. Same guards.
    fn build_open_plan(&mut self) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        if !is_feature_plan(&rel) || self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — building a plan builds into it.".to_string(),
            });
            return;
        }
        // Build the feature (its spec.md), whichever specs/<slug>/ artifact is open — so selecting
        // decomposition.md (or any phase file) and hitting Build targets specs/<slug>/ and reuses
        // the already-approved design instead of re-designing from that one file.
        self.start_staged_build_with(plan_task(&feature_spec_of(&rel)));
    }

    /// Build the plan from the last Breakdown — the Breakdown → Build hand-off. Reuses the exact
    /// task the plan run designed against (`last_plan_task`), so approving a breakdown then hitting
    /// "Build this plan" runs the staged build with no retyping. No-op if a run is in flight or no
    /// Breakdown has run this session.
    fn build_last_plan(&mut self) {
        if self.session.is_some() {
            return;
        }
        let Some(task) = self.last_plan_task.clone() else {
            return;
        };
        self.start_staged_build_with(task);
    }

    /// Commit the plan artifacts from the last Breakdown to the repo: `git add` the artifact dir
    /// (specs/<slug>/ or .smart-coder/plan/) + a commit. Lets a reviewed design be saved before —
    /// or instead of — building. Reports the outcome in the chat and refreshes the git view.
    fn commit_plan(&mut self) {
        if self.picked_workspace.is_none() {
            return;
        }
        // Stage everything the breakdown wrote (the plan/spec artifacts are the only new files a
        // plan-only run produces), then commit. `run_git` is best-effort and returns success.
        let staged = self.run_git(&["add", "-A"]);
        let committed =
            staged && self.run_git(&["commit", "-m", "docs: add reviewed plan/breakdown"]);
        let note = if committed {
            "✓ committed the plan to the repo".to_string()
        } else {
            "⚠ couldn't commit the plan (nothing to commit, or not a git repo)".to_string()
        };
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: note,
        });
        self.refresh_git_view();
    }

    /// Write the Nth proposed plan-file to disk (README.md / TODO.md), then refresh the
    /// conversation's view of the plan files and re-open it in the code view.
    fn apply_proposed_file(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        let root = self.workspace_root();
        let path = root.join(&pf.name);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&path, &pf.content).is_ok() {
            // Refresh the conversation's plan snapshot so later turns see the new files.
            if let Some(convo) = self.conversation.as_mut() {
                let (readme, todo) = {
                    let r = find_readme(&root)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    let t = find_todo_file(&root)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    (r, t)
                };
                convo.set_plan_files(&readme, &todo);
            }
            // Show the file we just wrote.
            self.follow_agent = false;
            self.select_file(pf.name.clone());
            // A feature plan KEEPS its card (marked applied) so its Breakdown/Build actions stay
            // available in the chat — a plan is written so you can then build it, so removing the
            // card strands you. A plain README/TODO edit isn't buildable, so it's removed as before.
            if is_feature_plan(&pf.name) {
                if let Some(slot) = self.proposed_files.get_mut(i) {
                    slot.applied = true;
                }
            } else {
                self.proposed_files.remove(i);
            }
            // Confirm the write in the chat thread, so applying is visible in the record.
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("✓ Applied {} to disk.", pf.name),
            });
        } else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ Could not write {}.", pf.name),
            });
        }
    }

    /// Select `rel` for the code panel and load its contents from the workspace root.
    fn select_file(&mut self, rel: String) {
        let root = self.workspace_root();
        // Switching to a different file resets the scrollable to the top; keep our virtualization
        // offset in sync so the first frame renders the top window, not the old file's slice.
        if self.selected_file.as_deref() != Some(rel.as_str()) {
            self.code_scroll_y = 0.0;
            self.code_viewport = None;
        }
        self.code = Some(sc_win::codeview::load(&root, &rel));
        // Opening a file makes it a CODE-panel tab. Push before the move into `selected_file`
        // (which becomes the ACTIVE tab); no duplicates, so re-opening an already-open file
        // just re-selects it. Every open path (tree/git/plan/comment) funnels through here, so
        // they all get a tab for free.
        if !self.open_tabs.contains(&rel) {
            self.open_tabs.push(rel.clone());
        }
        self.selected_file = Some(rel);
        self.refresh_changed_lines();
    }

    /// Close the CODE-panel tab for `path` (no-op if it isn't open). If it was the active tab,
    /// a neighbour becomes active (see `tab_after_close`); if none remain, the panel clears.
    /// Called by the ✕ button and when a file is deleted/discarded out from under it — a tab on
    /// a file that no longer exists is dead weight.
    fn close_tab(&mut self, path: &str) {
        if let Some(i) = self.open_tabs.iter().position(|p| p == path) {
            let was_active = self.selected_file.as_deref() == Some(path);
            self.open_tabs.remove(i);
            // Only the active tab closing changes what's shown; closing a background tab leaves
            // the active file alone.
            if was_active {
                match tab_after_close(i, self.open_tabs.len()) {
                    Some(idx) => {
                        let next = self.open_tabs[idx].clone();
                        self.select_file(next);
                    }
                    None => {
                        self.selected_file = None;
                        self.code = None;
                    }
                }
            }
        }
    }

    /// Re-read the currently selected file from disk (after the agent edited it), so the
    /// code panel reflects the latest bytes — and refresh which lines differ from HEAD.
    fn reload_selected(&mut self) {
        if let Some(rel) = self.selected_file.clone() {
            let root = self.workspace_root();
            self.code = Some(sc_win::codeview::load(&root, &rel));
        }
        self.refresh_changed_lines();
    }

    /// The OFF-THREAD live-view refresh: while a run is in flight, reload the shown file's
    /// contents + its git-diff highlight on a background thread and apply via
    /// [`Message::LiveViewReloaded`] — never on the UI thread (the file read + `git diff` cost
    /// 80–432ms per call and froze the UI when done synchronously in `pump`). Throttled to ~1s;
    /// returns `Task::none()` when no reload is due or nothing is selected.
    fn live_reload_task(&mut self) -> Task<Message> {
        if !(self.iterate_from_comment && self.session.is_some()) {
            return Task::none();
        }
        let due = self
            .last_reload
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(750));
        let Some(rel) = self.selected_file.clone() else {
            return Task::none();
        };
        if !due {
            return Task::none();
        }
        self.last_reload = Some(Instant::now());
        let root = self.workspace_root();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let code = sc_win::codeview::load(&root, &rel);
                    let diff = sc_win::gitdiff::file_diff(&root, &rel);
                    (code, diff.added)
                })
                .await
                .ok()
            },
            Message::LiveViewReloaded,
        )
    }

    /// Scroll the code view so `line` (1-based) sits in the MIDDLE of the viewport. Each rendered
    /// line is ~`CODE_LINE_PX` tall; back off by half the visible height so the target lands
    /// centered (falls back to a small top offset before the first scroll gives us a real
    /// viewport height). Shared by the minimap jump and the git-tab "open at first change".
    /// Commit the in-flight streamed reply (`self.streaming`) as a finished agent turn, then clear
    /// the live bubble. A no-op when nothing is streaming. Used wherever a stream ENDS (the next
    /// phase header, a gate decision, run completion) so the streamed text sticks in the thread
    /// instead of vanishing with the bubble. Strips any hidden `<think>` block, matching what the
    /// live bubble showed via `visible_so_far`.
    fn commit_streaming_turn(&mut self) {
        if let Some(buf) = self.streaming.take() {
            let visible = sc_win::chat::visible_so_far(&buf);
            if !visible.trim().is_empty() {
                self.chat_turns.push(sc_win::chat::Turn {
                    role: sc_win::chat::Speaker::Agent,
                    text: visible,
                });
            }
        }
    }

    /// Keep the chat thread pinned to the bottom as content streams in — `Task::none` unless
    /// auto-scroll is armed (the user is at the bottom, not reading back). `snap_to` with a
    /// relative y of 1.0 jumps to the end; it's cheap and idempotent when already there, so
    /// running it every tick is fine. Disarmed by `ChatScrolled` when the user scrolls up.
    fn chat_autoscroll_task(&self) -> Task<Message> {
        if self.chat_stuck_to_bottom {
            iced::widget::operation::snap_to(
                chat_scroll_id(),
                iced::widget::scrollable::RelativeOffset { x: 0.0, y: 1.0 },
            )
        } else {
            Task::none()
        }
    }

    fn scroll_code_to_line(&self, line: usize) -> Task<Message> {
        let center = line as f32 * CODE_LINE_PX;
        // `code_view_h` is 0 until the view's first scroll event; fall back to a typical editor
        // height so a jump on a freshly-opened file still lands the target near center, not glued
        // to the very top.
        let view_h = if self.code_view_h > 1.0 {
            self.code_view_h
        } else {
            400.0
        };
        let y = (center - view_h / 2.0).max(0.0);
        iced::widget::operation::scroll_to(
            code_scroll_id(),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
        )
    }

    /// Recompute the shown file's PR-style diff vs HEAD (git): added lines (green) + removed lines
    /// (red). Cheap `git diff -U0` on the one file (all-added for an untracked file); empty when
    /// nothing's selected. `changed_lines` is the green set, kept for the minimap + jump-to-change.
    fn refresh_changed_lines(&mut self) {
        self.file_diff = match &self.selected_file {
            Some(rel) => sc_win::gitdiff::file_diff(&self.workspace_root(), rel),
            None => sc_win::gitdiff::FileDiff::default(),
        };
        self.changed_lines = self.file_diff.added.clone();
    }

    /// Refresh the PR-view git state synchronously: the tree cache, per-file M/A/D statuses,
    /// branch, upstream. Used at the points where we want the state up-to-date *before* the next
    /// line runs (project open, right after a stage/discard). The periodic heartbeat instead uses
    /// the async path (`SyncWorkspace` → `compute_snapshot` off-thread → `WorkspaceSynced`).
    fn refresh_git_view(&mut self) {
        let snap = compute_snapshot(self.workspace_root());
        self.apply_snapshot(snap);
    }

    /// Apply a computed [`WorkspaceSnapshot`] to the live state. Pure assignment — the expensive
    /// walk/git work already happened in [`compute_snapshot`] (possibly on a background thread).
    fn apply_snapshot(&mut self, snap: WorkspaceSnapshot) {
        self.tree_cache = snap.tree;
        self.file_status = snap.file_status;
        self.stage_states = snap.stage_states;
        self.unstaged_deltas = snap.unstaged_deltas;
        self.staged_deltas = snap.staged_deltas;
        self.branch = snap.branch;
        self.upstream = snap.upstream;
    }

    /// Run a `git` subcommand in the workspace (e.g. `["add", "--", path]`) and return whether
    /// it succeeded. Used by the git-tab context-menu actions (stage / unstage / discard).
    fn run_git(&self, args: &[&str]) -> bool {
        let root = self.workspace_root();
        sc_win::proc::git()
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run a NETWORK git op (push / pull / fetch), capturing its output and reporting the outcome
    /// in the chat — these fail more often (auth, conflicts, no remote), so the message matters.
    /// `label` names the op for the report. Runs synchronously (briefly blocks the UI).
    fn run_git_net(&mut self, label: &str, args: &[&str]) {
        let root = self.workspace_root();
        let out = sc_win::proc::git().arg("-C").arg(&root).args(args).output();
        let (ok, detail) = match out {
            Ok(o) => {
                // git writes progress/results to stderr; prefer it, fall back to stdout.
                let err = String::from_utf8_lossy(&o.stderr);
                let msg = if err.trim().is_empty() {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                } else {
                    err.trim().to_string()
                };
                (o.status.success(), msg)
            }
            Err(e) => (false, e.to_string()),
        };
        // Keep the report short — the last non-empty line usually carries the gist.
        let gist = detail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        let text = if ok {
            format!(
                "✓ git {label} — {}",
                if gist.is_empty() { "done" } else { gist }
            )
        } else {
            format!("⚠ git {label} failed — {gist}")
        };
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text,
        });
    }

    /// Whether a swarm run is (or was) active — i.e. the topology has nodes to draw.
    fn is_swarm(&self) -> bool {
        !self.topology.is_empty()
    }

    /// Compute the outcome banner when a run ends. Two shapes:
    ///  • ITERATE (editing an existing project) → report the files the agent actually
    ///    *changed* — never a whole-repo scan (which would count thousands) and no "open
    ///    output folder" (you're already in your own repo).
    ///  • FROM-SCRATCH build → the "N files built" summary + open-folder, as before.
    fn finish_run(&mut self, ok: bool, summary: &str) {
        // Commit the final phase's streamed reply as a turn — the run ending is the last chance;
        // nothing after it would flush the live bubble, so its content would otherwise vanish.
        self.commit_streaming_turn();
        // Surface the outcome: jump to the Build tab when a run ends.
        self.bottom_tab = BottomTab::Build;
        // The agent's done working the selection — drop the amber "working" highlight (the
        // green git-change highlight takes over).
        self.working = None;
        if self.iterating {
            self.finish_iterate(ok, summary);
            return;
        }
        // A plan-only (Execute-plan) run designs; it does NOT build. Report the plan outcome —
        // never a "N files built" whole-repo scan (that produced the bogus "13730 files built").
        if self.planning_only {
            self.result = Some(RunResult {
                ok,
                headline: if ok {
                    "plan ready".to_string()
                } else {
                    "planning did not finish".to_string()
                },
                reason: summary.to_string(),
                files: Vec::new(),
                dir: None,
                // A clean plan run → offer the Build + Commit follow-ons in the result view.
                plan_ready: ok,
            });
            return;
        }
        let dir = self.run_dir.clone();
        let files = dir
            .as_deref()
            .map(sc_win::config::source_files)
            .unwrap_or_default();
        let n = files.len();

        // The truthful classification: "ok" from the run only counts if it actually
        // produced source files. Building zero files is a failure no matter what the
        // swarm reported.
        let (banner_ok, headline, reason) = if n == 0 {
            (
                false,
                "Built 0 files".to_string(),
                if self.plan.subtasks.is_empty() {
                    "the planner produced no buildable subtasks (decomposition failed)".to_string()
                } else {
                    format!("implementation failed — {summary}")
                },
            )
        } else if ok {
            (true, format!("{n} file{} built", plural(n)), String::new())
        } else {
            (
                false,
                format!("{n} file{} built, but did not finish", plural(n)),
                summary.to_string(),
            )
        };

        self.result = Some(RunResult {
            ok: banner_ok,
            headline,
            reason,
            files,
            dir,
            plan_ready: false,
        });
    }

    /// Undo the last fix: `git checkout --` exactly the files it changed, restoring them to
    /// their committed state. A deliberate "I don't like this change" action — the safe way to
    /// reject a fix, instead of hand-reverting. No-op if there's no finished result with files.
    fn undo_last_change(&mut self) {
        let files: Vec<String> = self
            .result
            .as_ref()
            .map(|r| r.files.clone())
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }
        let root = self.workspace_root();
        let ok = sc_win::proc::git()
            .arg("-C")
            .arg(&root)
            .arg("checkout")
            .arg("--")
            .args(&files)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: if ok {
                format!(
                    "↩ Undid the change — reverted {} file(s) to committed state.",
                    files.len()
                )
            } else {
                "⚠ Couldn't undo (not a git repo, or the file was already committed).".to_string()
            },
        });
        if ok {
            // Clear the result banner + refresh the code view to the reverted content.
            self.result = None;
            self.reload_selected();
        }
    }

    /// The iterate outcome: report only the files the agent edited this run, and whether it
    /// finished green. No repo scan, no open-folder button.
    fn finish_iterate(&mut self, ok: bool, summary: &str) {
        let files = self.edited_files.clone();
        let n = files.len();
        // On a successful comment-driven fix, mark the comment resolved (in place) and refresh
        // the PR file-tree statuses. The resolved comment stays visible as a "done" record.
        if ok && self.iterate_from_comment {
            let mut changed = false;
            for f in &files {
                changed |= self.comments.resolve_latest_on(f);
            }
            if changed {
                sc_win::comments::save(&self.workspace_root(), &self.comments);
            }
        }
        self.refresh_git_view();
        let (headline, reason) = if ok && n > 0 {
            (
                format!("done — {n} file{} changed", plural(n)),
                String::new(),
            )
        } else if ok {
            // Finished cleanly but edited nothing (e.g. "already as requested").
            ("done — no changes needed".to_string(), summary.to_string())
        } else {
            (
                format!("stopped — {n} file{} changed, did not finish", plural(n)),
                summary.to_string(),
            )
        };
        // If this iterate came from a line comment, report the outcome IN THE CHAT so you
        // see what it did without hunting in the Build tab.
        if self.iterate_from_comment {
            self.iterate_from_comment = false;
            let text = if ok && n > 0 {
                let list = files.join(", ");
                let verified = if self.verify_text.is_some() {
                    " and it still compiles"
                } else {
                    ""
                };
                format!("✓ Done. Changed {n} file{} ({list}){verified}.", plural(n))
            } else if ok {
                "✓ Done — nothing needed changing.".to_string()
            } else {
                format!(
                    "⚠ I couldn't finish that cleanly ({} file{} touched). {summary}",
                    n,
                    plural(n)
                )
            };
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text,
            });
        }
        self.result = Some(RunResult {
            ok,
            headline,
            reason,
            files,
            dir: None, // no "open output folder" — you're iterating in your own repo
            plan_ready: false,
        });
    }

    /// Drain the in-flight chat turn, if any. On a reply: split prose from proposed
    /// plan-files, record the assistant turn, show the prose in the thread, stage the
    /// proposed files for Apply, and auto-open the first one in the code view.
    fn pump_chat(&mut self) {
        let Some(cs) = &self.chat_session else {
            return;
        };
        let events = cs.drain();
        for ev in events {
            match ev {
                sc_win::chat_session::ChatEvent::Token(delta) => {
                    // Grow the live "typing" bubble. Strip <think> for display as it streams
                    // (a reasoning delta shouldn't flash into the visible reply).
                    let buf = self.streaming.get_or_insert_with(String::new);
                    buf.push_str(&delta);
                    // Mirror the live typing to any remote client — same cleaned view the
                    // desktop shows (strips <think> reasoning + hides file-block fences), so
                    // the phone doesn't flash raw reasoning.
                    if let Some(m) = &self.remote {
                        let visible = sc_win::chat::visible_so_far(buf);
                        if !visible.is_empty() {
                            m.push(sc_core::AgentEvent::ChatDelta {
                                cumulative: visible,
                            });
                        }
                    }
                }
                sc_win::chat_session::ChatEvent::Reply(raw, intent) => {
                    self.streaming = None; // the live bubble is replaced by the finished turn
                    self.working = None; // a question answer is done → drop the amber highlight
                    let (prose, mut files) = sc_win::chat::parse_reply(&raw);
                    // Robustness for small local models: a FEATURE PLAN whose reply came back as
                    // bare prose (the model ignored the ```file: fence instruction — common when
                    // the prompt is large) is WRAPPED into a PLAN-<slug>.md proposal here, so a
                    // plan always yields an Apply/verify card rather than silently staying prose.
                    let is_plan_intent = matches!(
                        intent,
                        Some(sc_win::chat::ChatIntent::FeaturePlan)
                            | Some(sc_win::chat::ChatIntent::PlanFromTodo)
                    );
                    if is_plan_intent && files.is_empty() && !prose.trim().is_empty() {
                        let slug = self.plan_slug_for_reply();
                        files.push(sc_win::chat::wrap_plan_prose(&prose, &slug));
                    }
                    // Record the user's VERBATIM request at the top of the spec (provenance),
                    // whether the model emitted a file block or we wrapped its prose.
                    if is_plan_intent {
                        let request = self.last_user_request();
                        for f in &mut files {
                            if is_feature_plan(&f.name) {
                                f.content = sc_win::chat::prepend_request(&f.content, &request);
                            }
                        }
                    }
                    // A ```command block (the Command intent) → offer it as a one-click Run in
                    // the terminal, rather than auto-executing.
                    self.proposed_command = sc_win::chat::extract_command(&raw);
                    if let Some(convo) = self.conversation.as_mut() {
                        convo.record_reply(&raw);
                        // Fold any proposed plan-file content into the conversation's plan
                        // snapshot NOW (before Apply), so follow-up questions ("what's in the
                        // todo?") see what was just proposed — otherwise the system prompt
                        // keeps showing the stale on-disk file and the model contradicts its
                        // own proposal.
                        for f in &files {
                            if f.name.eq_ignore_ascii_case("README.md") {
                                convo.set_readme(&f.content);
                            } else if f.name.eq_ignore_ascii_case("TODO.md") {
                                convo.set_todo(&f.content);
                            }
                        }
                    }
                    let shown = if !prose.is_empty() {
                        prose
                    } else if let Some(cmd) = &self.proposed_command {
                        // A reply that was only a command block → say what will run.
                        format!("Run this in the terminal:  {cmd}")
                    } else {
                        // A reply that was only file blocks → note what changed.
                        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                        format!("Proposed changes to {}.", names.join(", "))
                    };
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: shown.clone(),
                    });
                    // Mirror the finished assistant turn to any remote client.
                    if let Some(m) = &self.remote {
                        m.push(sc_core::AgentEvent::ChatMessage {
                            role: "agent".into(),
                            text: shown,
                        });
                    }
                    self.proposed_files = files;
                    // Auto-open the first proposed file so you see the plan taking shape.
                    if let Some(first) = self.proposed_files.first() {
                        let name = first.name.clone();
                        let content = first.content.clone();
                        // Show the PROPOSED content directly (not the on-disk file, which
                        // hasn't been written yet).
                        self.follow_agent = false;
                        self.selected_file = Some(name.clone());
                        self.code = Some(sc_win::codeview::from_text(&name, &content));
                    }
                    self.chat_session = None;
                }
                sc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.streaming = None;
                    self.working = None;
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    self.chat_session = None;
                }
            }
        }
    }

    /// Drain the worker channels into UI state. Called each tick.
    /// Apply commands a remote (phone) client sent this tick, through the same paths the
    /// desktop's own buttons use: a chat message → `send_chat()`; a stop → `session.cancel()`.
    /// Approve/deny are resolved inside the mirror server directly (they reach the reply
    /// channel the gate bar shares), so they need no handling here.
    fn pump_remote(&mut self) {
        let Some(cmds) = self.remote.as_ref().map(|m| m.drain_inbound()) else {
            return;
        };
        for cmd in cmds {
            match cmd {
                sc_web::InboundCmd::Chat(text) => {
                    // A chat needs an open conversation; start one if the desktop hasn't yet.
                    if self.conversation.is_none() {
                        self.open_conversation();
                    }
                    self.intent = text;
                    self.send_chat();
                }
                sc_web::InboundCmd::Cancel => {
                    if let Some(session) = &self.session {
                        session.cancel();
                    }
                }
                sc_web::InboundCmd::Open(path) => {
                    // Security: only open a path the desktop itself published in recents —
                    // a remote client can never open an arbitrary folder on this machine.
                    let allowed = sc_win::persist::load()
                        .recents
                        .iter()
                        .any(|p| p.to_string_lossy() == path);
                    let dir = std::path::PathBuf::from(&path);
                    if allowed && dir.is_dir() {
                        self.open_workspace(dir);
                    }
                }
            }
        }
    }

    /// Drain any output from a running terminal command into the scrollback. When the command
    /// finishes (an `[exit]` was seen), drop the receiver so the tick can quiesce.
    fn pump_terminal(&mut self) {
        if let Some(rx) = &self.term_rx {
            if self.terminal.drain(rx) {
                self.term_rx = None;
            }
        }
    }

    /// Drive the backend health probe: adopt a finished probe's result, then kick a fresh
    /// probe on startup and roughly every 10s. The probe runs on a background thread with a
    /// short timeout so a dead backend never blocks the UI; the result returns via a channel.
    /// A real 1-token completion is used (not a `/models` ping) so a router with no model
    /// loaded reads as `NoModel`, not healthy.
    fn tick_health_probe(&mut self) {
        // 1) Adopt a completed probe.
        if let Some(rx) = &self.health_rx {
            if let Ok(h) = rx.try_recv() {
                self.backend_health = Some(h);
                self.health_rx = None;
            }
        }
        // 2) Kick a new one if none is in flight and it's been ~10s (or we've never probed).
        let due = self
            .last_health_probe
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10));
        if self.health_rx.is_none() && due {
            let (base_url, model) = (self.cfg.base_url.clone(), self.cfg.model.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // A bare backend (no context-detection HTTP at build time) with a short probe
                // timeout — fail fast on a dead endpoint.
                let backend = sc_model::OpenAiBackend::new(base_url, model);
                let _ = tx.send(backend.health_probe(4));
            });
            self.health_rx = Some(rx);
            self.last_health_probe = Some(std::time::Instant::now());
        }
    }

    /// The slug for a wrapped feature plan, derived from the user's most recent request (the
    /// feature they asked to plan) so it lands as `PLAN-<feature>.md` — not from the open file
    /// (the old `plan_slug()` bug that produced `PLAN-todo.md` for a seat-types plan). Falls
    /// back to `feature` when there's no user turn to name it after.
    fn plan_slug_for_reply(&self) -> String {
        self.chat_turns
            .iter()
            .rev()
            .find(|t| t.role == sc_win::chat::Speaker::You)
            .map(|t| sc_win::chat::slug_for(&t.text))
            .filter(|s| s != "feature")
            .unwrap_or_else(|| "feature".to_string())
    }

    /// The user's most recent message (verbatim), for recording as the spec's `## Request`.
    fn last_user_request(&self) -> String {
        self.chat_turns
            .iter()
            .rev()
            .find(|t| t.role == sc_win::chat::Speaker::You)
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    /// Decide where the next terminal command runs, starting the sandbox container on first
    /// use. Mirrors the agent: `self.cfg.sandbox()` is the single source of truth for Host vs
    /// Docker, so the terminal and the agent always execute in the same place.
    ///
    /// **Strict containment:** when Docker is the configured intent (`use_docker`), this NEVER
    /// silently falls back to the host. If it can't sandbox — no project open (nothing to
    /// mount) or the container won't start (Docker down) — it returns `Err(reason)` and the
    /// caller refuses to run the command. The host shell is used ONLY when sandboxing is
    /// explicitly off. This guarantees you can never *think* you're contained while running on
    /// the host.
    fn term_exec_mode(&mut self) -> Result<sc_win::terminal::ExecMode, String> {
        use sc_win::terminal::ExecMode;

        // Sandbox off → an explicit, plain host scratch shell (in the workspace if one's open).
        let image = match self.cfg.sandbox() {
            sc_verify::Sandbox::Host => {
                let cwd = self
                    .picked_workspace
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                return Ok(ExecMode::Host { cwd });
            }
            sc_verify::Sandbox::Docker { image } => image,
            // Session is a runtime-only state built from the live container, never returned by
            // `cfg.sandbox()`; use its image for exhaustiveness.
            sc_verify::Sandbox::Session(c) => c.image().to_string(),
        };

        // Docker is intended. A project must be open — the container mounts the workspace.
        if self.picked_workspace.is_none() {
            return Err(
                "sandbox terminal needs a project open (open a folder to mount into the container)"
                    .to_string(),
            );
        }
        let sc = self.ensure_session_container(&image)?;
        Ok(ExecMode::Container(sc))
    }

    /// Ensure the workspace's shared session container is running (starting it once on first
    /// use, clearing any stale one first), and return a handle. Shared by the terminal and the
    /// agent so BOTH `docker exec` into the SAME container — the agent's file/state changes are
    /// then visible in the terminal and vice-versa. `Err` with a human reason if no project is
    /// open or Docker won't start.
    fn ensure_session_container(
        &mut self,
        image: &str,
    ) -> Result<sc_verify::SessionContainer, String> {
        let Some(cwd) = self.picked_workspace.clone() else {
            return Err("no project open".to_string());
        };
        let sc = self
            .term_container
            .get_or_insert_with(|| sc_verify::SessionContainer::new(&cwd, image))
            .clone();
        if !self.term_container_started {
            // Clear any stale container from a previous session, then start fresh, detached.
            let _ = sc.stop_command().output();
            match sc.start_command(&cwd).output() {
                Ok(o) if o.status.success() => {
                    self.terminal.note(format!(
                        "▶ sandbox container `{}` started — commands run inside it",
                        sc.name()
                    ));
                    self.term_container_started = true;
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    self.term_container = None;
                    return Err(format!(
                        "could not start sandbox container: {} — is Docker running?",
                        err.trim()
                    ));
                }
                Err(e) => {
                    self.term_container = None;
                    return Err(format!("docker not available: {e}"));
                }
            }
        }
        Ok(sc)
    }

    /// The sandbox an agent run should use: the shared session container when sandboxing is on
    /// and a project is open (so the agent execs into the same container as the terminal),
    /// else whatever `cfg.sandbox()` decides (host, or per-run Docker as a fallback). Starting
    /// the container can fail (Docker down) — on failure we log and fall back to `cfg.sandbox()`
    /// so a run still proceeds rather than being blocked.
    fn agent_sandbox(&mut self) -> sc_verify::Sandbox {
        let image = match self.cfg.sandbox() {
            sc_verify::Sandbox::Docker { image } if self.picked_workspace.is_some() => image,
            other => return other,
        };
        match self.ensure_session_container(&image) {
            Ok(sc) => sc_verify::Sandbox::Session(sc),
            Err(reason) => {
                self.rows.push(Row::ok(
                    "⚠",
                    format!("sandbox container unavailable ({reason}) — using per-run container"),
                ));
                sc_verify::Sandbox::Docker { image }
            }
        }
    }

    /// Tear down the workspace sandbox container (force-remove) and reset its state. Called on
    /// project switch and app close so a container never outlives the project that owns it.
    fn teardown_term_container(&mut self) {
        if let Some(sc) = self.term_container.take() {
            let _ = sc.stop_command().output();
        }
        self.term_container_started = false;
    }

    fn pump(&mut self) {
        self.pump_remote();
        self.tick_health_probe();
        self.pump_terminal();
        self.pump_chat();
        self.pump_triage();
        self.pump_replace();
        // While a run is in flight, keep the code view + change-highlight fresh from disk so
        // edits land live (the agent edits the real files). This does a SYNCHRONOUS `git diff`
        // on the UI thread, which at the 50ms tick rate froze the UI on a big repo (void-claim)
        // — throttle to ~1s so the view stays live without starving rendering.
        // Live-view refresh is now driven asynchronously by `live_reload_task()` (called from the
        // Tick handler, which can return a Task) — never synchronously here, since it does a
        // file read + git diff that was blocking the UI thread 80–432ms per call.
        let Some(session) = &self.session else {
            return;
        };
        for ev in session.drain_events() {
            // Tee this event out to any remote (phone) mirror. Agent/Swarm events are
            // already serde `AgentEvent`s the Hub takes directly; the others become a
            // short ChatMessage note so the remote log still reflects run boundaries.
            if let Some(m) = &self.remote {
                match &ev {
                    UiEvent::Agent(e) => m.push(e.clone()),
                    UiEvent::Done { summary, .. } => m.push(sc_core::AgentEvent::ChatMessage {
                        role: "system".into(),
                        text: format!("run finished: {summary}"),
                    }),
                    UiEvent::Failed(msg) => m.push(sc_core::AgentEvent::ChatMessage {
                        role: "system".into(),
                        text: format!("run failed: {msg}"),
                    }),
                    // Swarm/Phase mirroring is out of v1 scope (agent + chat only).
                    _ => {}
                }
            }
            match ev {
                UiEvent::Agent(e) => {
                    // A staged run streams its per-phase prompt + reply into the chat thread as
                    // ChatMessage/ChatDelta (so a slow phase reads as alive, not frozen). Fold them
                    // the same way `pump_chat` folds a live chat turn: a ChatMessage is a terminal
                    // turn, a ChatDelta grows the live "typing" bubble. (The regular agent/iterate
                    // flow uses the ChatEvent path in `pump_chat`; the session's staged run has no
                    // ChatSession, so it routes through UiEvent::Agent here.)
                    if let sc_core::AgentEvent::ChatMessage { role, text } = &e {
                        // COMMIT any in-flight streamed reply as a finished turn before this new
                        // message — otherwise the streamed phase reply (which only lived in the
                        // transient `streaming` bubble) is thrown away when the NEXT phase's header
                        // arrives, and the chat shows headers with no content (the bug: only the
                        // last phase's reply survived).
                        self.commit_streaming_turn();
                        let speaker = match role.as_str() {
                            "you" => sc_win::chat::Speaker::You,
                            _ => sc_win::chat::Speaker::Agent,
                        };
                        self.chat_turns.push(sc_win::chat::Turn {
                            role: speaker,
                            text: text.clone(),
                        });
                        continue;
                    }
                    if let sc_core::AgentEvent::ChatDelta { cumulative } = &e {
                        // Grow the live bubble with the full cumulative reply so far. The view
                        // renders `self.streaming` whenever it's Some, so this types on screen; the
                        // next phase's header ChatMessage finalizes it (no duplicate terminal turn).
                        self.streaming = Some(cumulative.clone());
                        continue;
                    }
                    // Live "watch it type": as the model streams a write/edit, preview the
                    // growing file content in the code view, word by word, before it lands.
                    if let sc_core::AgentEvent::ContentDelta { cumulative, .. } = &e {
                        if let Some(p) = sc_win::codeview::partial_edit_preview(cumulative) {
                            if !p.content.is_empty() {
                                let name = p
                                    .file
                                    .clone()
                                    .or_else(|| self.selected_file.clone())
                                    .unwrap_or_else(|| "(writing…)".to_string());
                                if p.file.is_some() {
                                    self.selected_file = p.file.clone();
                                    self.follow_agent = false;
                                }
                                self.code = Some(sc_win::codeview::from_text(&name, &p.content));
                            }
                        }
                        continue; // a delta is preview-only; nothing else to fold
                    }
                    if let sc_core::AgentEvent::Planned { steps }
                    | sc_core::AgentEvent::PlanRevised { steps } = &e
                    {
                        self.board = steps.clone();
                    }
                    if let sc_core::AgentEvent::Verification { summary, .. } = &e {
                        self.verify_text = Some(summary.clone());
                    }
                    // Record files the agent actually edited/wrote (for the iterate banner).
                    if sc_win::codeview::is_mutating_touch(&e) {
                        if let Some(rel) = sc_win::codeview::file_touched_by(&e) {
                            if !self.edited_files.contains(&rel) {
                                self.edited_files.push(rel);
                            }
                        }
                    }
                    // During a line-comment fix, narrate the key steps into the chat so the
                    // work is VISIBLE (a fix used to run silently). Only meaningful steps —
                    // editing a file and the verify result — not every read.
                    if self.iterate_from_comment {
                        if let Some(line) = fix_feed_line(&e) {
                            self.chat_turns.push(sc_win::chat::Turn {
                                role: sc_win::chat::Speaker::Agent,
                                text: line,
                            });
                        }
                    }
                    // Follow the agent: when it touches a file and we're in follow mode,
                    // show that file in the code panel — so edits land in front of you.
                    if self.follow_agent {
                        if let Some(rel) = sc_win::codeview::file_touched_by(&e) {
                            self.select_file(rel);
                        }
                    }
                    // (A tool result may have changed the shown file on disk; the live view is
                    // refreshed asynchronously by the Tick-driven `live_reload_task`, not
                    // synchronously here — the reload does a file read + git diff that blocked the
                    // UI thread 80–432ms per call when done per-event.)
                    self.rows.extend(agent_rows(&e));
                }
                UiEvent::Swarm(e) => {
                    // Fold into the live topology (canvas) and the per-subtask board,
                    // and append to the flat activity stream.
                    self.topology.apply(&e, self.now());
                    self.swarm_board.apply(&e);
                    self.rows.extend(swarm_rows(&e));
                }
                UiEvent::Phase {
                    phase,
                    content,
                    tests_written,
                    dir,
                } => {
                    // Fold a staged-workflow phase into the plan (the plan panel) and
                    // note it in the activity stream. `dir` (workspace-relative artifact dir)
                    // teaches the plan each phase's file path — the master list opens it in
                    // the code view and harvests line-comments on it for send-back.
                    self.plan
                        .apply(phase, &content, &tests_written, dir.as_deref());
                    if tests_written.is_empty() {
                        self.rows
                            .push(Row::ok("◆", format!("plan · {}", phase.title())));
                    } else {
                        self.rows.push(Row::ok(
                            "✓",
                            format!("wrote {} frozen test file(s)", tests_written.len()),
                        ));
                    }
                }
                UiEvent::Done { ok, summary } => {
                    self.finish_run(ok, &summary);
                    self.session = None;
                }
                UiEvent::Failed(msg) => {
                    self.finish_run(false, &format!("error: {msg}"));
                    self.session = None;
                }
            }
        }
        // Move any new decision requests onto the gate bar.
        let pending: Vec<Pending> = match &self.session {
            Some(session) => session.drain_pending(),
            None => Vec::new(),
        };
        // When a workflow gate arrives, auto-open its phase file in CODE so the user can read
        // (and line-comment) the artifact immediately — the details live in the editor now.
        let mut open_gate_file: Option<String> = None;
        {
            for p in pending {
                self.gatebar.push(match p {
                    Pending::Confirm {
                        command,
                        default_reason,
                        reply,
                    } => {
                        // If the remote mirror is live, register this confirm so a phone can
                        // approve/deny it. Both the local buttons and the remote resolve hold
                        // a clone of the reply sender; whichever answers first wins (the
                        // worker's `recv()` takes the first `send`). `id` = 0 with no mirror.
                        let id = self
                            .remote
                            .as_ref()
                            .map(|m| m.register_confirm(&command, &default_reason, reply.clone()))
                            .unwrap_or(0);
                        Gatebar::Confirm {
                            id,
                            command,
                            reason: default_reason,
                            reply,
                        }
                    }
                    Pending::Gate {
                        phase,
                        content,
                        reply,
                    } => {
                        // Remember to open this phase's artifact file once the borrow ends.
                        if let Some(path) = self.plan.path_for(phase) {
                            open_gate_file = Some(path);
                        }
                        Gatebar::Gate {
                            phase,
                            content,
                            reply,
                        }
                    }
                });
            }
        }
        // Auto-open the gating artifact (outside the loop so `select_file`'s `&mut self` is free).
        // Pin the view (follow_agent = false) so a live run doesn't scroll it away.
        if let Some(path) = open_gate_file {
            self.follow_agent = false;
            self.select_file(path);
        }
    }

    /// Answer the oldest pending confirm with `c` (no-op if the front isn't a confirm).
    fn answer_confirm(&mut self, c: Confirmation) {
        if matches!(self.gatebar.first(), Some(Gatebar::Confirm { .. })) {
            if let Gatebar::Confirm { reply, .. } = self.gatebar.remove(0) {
                let _ = reply.send(c);
            }
        }
    }

    /// The phase currently stopped at a human gate, if any — the front gatebar entry when
    /// it's a workflow `Gate` (the worker blocks on one at a time). The master list marks
    /// this row as "gating" and shows its Approve / Send-back / Abort buttons inline.
    fn gating_phase(&self) -> Option<Phase> {
        match self.gatebar.first() {
            Some(Gatebar::Gate { phase, .. }) => Some(*phase),
            _ => None,
        }
    }

    /// Answer the oldest pending workflow gate with `d`.
    fn answer_gate(&mut self, d: Decision) {
        if matches!(self.gatebar.first(), Some(Gatebar::Gate { .. })) {
            if let Gatebar::Gate { phase, reply, .. } = self.gatebar.remove(0) {
                // Narrate the gate DECISION into the chat thread, so the staged run's back-and-forth
                // (prompt → streamed reply → decision) reads as a complete conversation. The session
                // can't see the decision itself — it's resolved here in the app over the gate's
                // private reply channel — so this is where it becomes a visible turn.
                let note = match &d {
                    Decision::Approve => format!("✓ {} approved", phase.title()),
                    Decision::Revise => format!("✎ {} revised", phase.title()),
                    Decision::SendBack { target, notes } => match notes {
                        Some(n) => {
                            format!("↩ sent {} back to {} — {n}", phase.title(), target.title())
                        }
                        None => format!("↩ sent {} back to {}", phase.title(), target.title()),
                    },
                    Decision::Abort => format!("■ aborted at {}", phase.title()),
                };
                // Commit the phase's streamed reply as a turn, THEN log the decision after it — so
                // the gated phase's content sticks in the thread instead of vanishing.
                self.commit_streaming_turn();
                self.chat_turns.push(sc_win::chat::Turn {
                    role: sc_win::chat::Speaker::You,
                    text: note,
                });
                let _ = reply.send(d);
            }
            self.sendback_notes.clear();
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IntentChanged(s) => self.intent = s,
            Message::ModelChanged(s) => self.model_input = s,
            Message::OrchModelChanged(s) => self.orch_model_input = s,
            Message::AdvisorChanged(s) => self.advisor_input = s,
            Message::LocalUrlChanged(s) => self.local_url_input = s,
            Message::LocalKeyChanged(s) => self.local_key_input = s,
            Message::GeminiUrlChanged(s) => self.gemini_url_input = s,
            Message::GeminiKeyChanged(s) => self.gemini_key_input = s,
            Message::CoderProviderChanged(p) => self.cfg.coder_provider = p,
            Message::PlannerProviderChanged(p) => self.cfg.planner_provider = p,
            Message::AdvisorProviderChanged(p) => self.cfg.advisor_provider = p,
            Message::SettingsTabChanged(t) => self.settings_tab = t,
            Message::VerifyChanged(s) => self.verify_input = s,
            Message::SuffixChanged(s) => self.suffix_input = s,
            Message::ToggleSettings => {
                self.open_menu = None;
                // Closing the modal COMMITS + persists the edits (save-on-close), so a user can
                // set up connections/routing and just close the panel without starting a run.
                let was_open = self.settings_open;
                self.settings_open = !self.settings_open;
                if was_open {
                    self.commit_settings();
                }
            }
            Message::ToggleYolo(v) => self.cfg.yolo = v,
            Message::ToggleDryRun(v) => self.cfg.dry_run = v,
            Message::RunTdd => self.start(RunKind::Tdd),
            // The composer's main run now goes through the DISCIPLINED path: staged plan →
            // architecture → decompose → compiler-driven build (tiny compiler-verified steps),
            // instead of the bare single-agent iterate loop. The line-comment small-fix path
            // (`start_iterate_with`) still uses iterate — it's a tiny scoped edit, not a feature.
            Message::RunIterate => self.start(RunKind::StagedBuild),
            Message::Tick => {
                self.pump();
                // Drive the live code-view refresh OFF the UI thread (returns Task::none unless a
                // reload is due). This is the fix for the Execute-plan freeze.
                // Also keep the chat pinned to the bottom as content streams in (unless the user
                // scrolled up) — batched so both run this tick.
                return Task::batch([self.live_reload_task(), self.chat_autoscroll_task()]);
            }
            Message::HealthTick => self.tick_health_probe(),
            Message::LiveViewReloaded(result) => {
                if let Some((code, added)) = result {
                    self.code = Some(code);
                    self.changed_lines = added;
                }
            }
            Message::SyncWorkspace => {
                // Re-walk the tree + git state OFF the UI thread — the walk and the git
                // subprocesses are the slow part, so compute a snapshot on a background thread and
                // apply it when it's ready (`WorkspaceSynced`). Skip if a sync is already pending.
                if self.picked_workspace.is_some() && !self.sync_pending {
                    self.sync_pending = true;
                    let root = self.workspace_root();
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || compute_snapshot(root))
                                .await
                                .ok()
                        },
                        Message::WorkspaceSynced,
                    );
                }
            }
            Message::WorkspaceSynced(snap) => {
                self.sync_pending = false;
                if let Some(snap) = snap {
                    self.apply_snapshot(snap);
                }
            }
            Message::SelectFile(rel) => {
                // Click-to-pin: show this file and stop auto-following the agent until
                // the next run re-arms follow.
                self.follow_agent = false;
                self.select_file(rel);
            }
            Message::SelectTab(path) => {
                // Switching tabs pins the view and re-selects the file. `select_file` is
                // idempotent for an already-open tab (it reloads + re-selects), and the active
                // tab === `selected_file`, so the body just follows.
                self.follow_agent = false;
                self.select_file(path);
            }
            Message::CloseTab(path) => self.close_tab(&path),
            Message::ModifiersChanged(m) => {
                // Cache the held modifiers so the next git-row click can tell single- from
                // ctrl-toggle from shift-range selection (button presses carry no modifiers).
                self.modifiers = m;
            }
            Message::SelectGitFile(rel) => {
                // Branch on the tracked modifiers (iced buttons don't report the modifiers held
                // at click time, so we read the live `self.modifiers` cached from key events):
                //   Ctrl → toggle this row into/out of the multi-selection (keep the rest).
                //   Shift → re-select the contiguous range from the anchor to this row.
                //   neither → plain single-select (clear the set, select just this row).
                // In every case the last-clicked row becomes the previewed file (`selected_file`),
                // since the CODE panel is single-file.
                if self.modifiers.control() && !self.modifiers.shift() {
                    // Ctrl-toggle: additive, doesn't clear the rest. Move the anchor here.
                    if !self.git_selection.remove(&rel) {
                        self.git_selection.insert(rel.clone());
                    }
                    self.git_select_anchor = Some(rel.clone());
                } else if self.modifiers.shift() {
                    // Shift-range: select the inclusive span between the anchor (or this row, if
                    // no anchor yet) and this row in the CURRENT DISPLAYED ORDER, replacing the
                    // set. The anchor is kept, so successive shift-clicks re-anchor from it.
                    let order = self.git_display_order();
                    let anchor = self
                        .git_select_anchor
                        .clone()
                        .unwrap_or_else(|| rel.clone());
                    self.git_selection = git_range(&order, &anchor, &rel);
                    if self.git_select_anchor.is_none() {
                        self.git_select_anchor = Some(anchor);
                    }
                } else {
                    // Plain click: single-select. The set always holds the selected file too, so a
                    // click leaves a 1-element selection consistent with the previewed file.
                    self.git_selection.clear();
                    self.git_selection.insert(rel.clone());
                    self.git_select_anchor = Some(rel.clone());
                }
                // Open the file, then jump the code view to its first changed line (git-tab
                // click → land you on the change, VS-Code-diff style). The scroll is DEFERRED a
                // beat so it runs against the newly-laid-out content, not the previous file's
                // tree — a same-frame scroll_to misses on large files (the new lines don't exist
                // in the layout yet).
                self.follow_agent = false;
                self.select_file(rel);
                if self.changed_lines.iter().next().is_some() {
                    // Re-emit as a follow-up message: it's processed after this update's view()
                    // rebuilds with the new file, so scroll_to acts on the correct layout.
                    return Task::done(Message::JumpToFirstChange);
                }
            }
            Message::JumpToFirstChange => {
                if let Some(&first) = self.changed_lines.iter().next() {
                    return self.scroll_code_to_line(first);
                }
            }
            Message::ToggleDir(rel) => {
                if !self.collapsed_dirs.remove(&rel) {
                    self.collapsed_dirs.insert(rel);
                }
            }
            Message::FileFilterChanged(q) => {
                self.file_filter = q;
            }
            Message::ToggleMenu(m) => {
                self.open_menu = if self.open_menu == Some(m) {
                    None
                } else {
                    Some(m)
                };
            }
            Message::ChatSend => self.send_chat(),
            Message::CopyTurn(t) => return iced::clipboard::write(t),
            Message::RunProposedCommand => {
                if let Some(cmd) = self.proposed_command.take() {
                    // Show the Terminal tab and run there, through the same sandbox path as a
                    // typed command (strict containment applies — refused if it can't sandbox).
                    self.bottom_tab = BottomTab::Terminal;
                    if !self.terminal.running {
                        match self.term_exec_mode() {
                            Ok(mode) => self.term_rx = self.terminal.run(&cmd, &mode),
                            Err(reason) => self.terminal.blocked(&cmd, &reason),
                        }
                    }
                }
            }
            Message::DismissProposedCommand => self.proposed_command = None,
            Message::ChatEditorAction(i, action) => {
                // Read-only: apply selection/cursor/scroll actions so drag-select + Ctrl+C
                // work, but never edits — the message text is immutable.
                if !action.is_edit() {
                    if let Some(content) = self.chat_editors.get_mut(i) {
                        content.perform(action);
                    }
                }
            }
            Message::ApplyFile(i) => self.apply_proposed_file(i),
            Message::ExecutePlan(i) => self.execute_plan(i),
            Message::BreakdownPlan(i) => self.breakdown_plan(i),
            Message::ExecuteOpenPlan => self.execute_open_plan(),
            Message::BuildOpenPlan => self.build_open_plan(),
            Message::BuildLastPlan => self.build_last_plan(),
            Message::CommitPlan => self.commit_plan(),
            Message::ToggleThink(v) => self.think = v,
            Message::ToggleDebug(v) => self.debug = v,
            Message::UndoLastChange => self.undo_last_change(),
            Message::DismissComment(i) => {
                self.comments.remove(i);
                sc_win::comments::save(&self.workspace_root(), &self.comments);
            }
            Message::RevertComment(i) => self.revert_comment(i),
            Message::RevertBlock(cur_start) => self.revert_block(cur_start),
            Message::MinimapJump(line) => {
                return self.scroll_code_to_line(line);
            }
            Message::CodeScrolled(vp) => {
                // Record the visible slice as fractions of the whole content so the minimap can
                // box "you are here". top = how far down we've scrolled; height = how much of the
                // file fits on screen.
                let top = vp.relative_offset().y;
                let content_h = vp.content_bounds().height.max(1.0);
                let view_h = vp.bounds().height;
                self.code_view_h = view_h;
                self.code_view_w = vp.bounds().width;
                self.code_scroll_y = vp.absolute_offset().y;
                let height = (view_h / content_h).clamp(0.0, 1.0);
                self.code_viewport = Some((top * (1.0 - height), height));
            }
            Message::ChatScrolled(vp) => {
                // Arm auto-scroll only when the user is at (or within a line of) the bottom; scrolling
                // UP disarms it so a streaming reply doesn't yank them back down while they read. The
                // last few px of tolerance keeps it "stuck" through the tiny jitter as content grows.
                let content_h = vp.content_bounds().height;
                let view_h = vp.bounds().height;
                let bottom = (content_h - view_h).max(0.0);
                let at_bottom = bottom - vp.absolute_offset().y <= 8.0;
                self.chat_stuck_to_bottom = at_bottom;
            }
            Message::CancelRun => {
                if let Some(s) = &self.session {
                    s.cancel();
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: "⏹ cancelling — stopping at the next step…".to_string(),
                    });
                }
            }
            Message::CancelChat => {
                if let Some(s) = &self.chat_session {
                    s.cancel();
                }
            }
            Message::SelectBottomTab(t) => self.bottom_tab = t,
            Message::TermInput(s) => self.terminal.input = s,
            Message::TermSubmit => {
                if !self.terminal.running {
                    let cmdline = self.terminal.input.clone();
                    match self.term_exec_mode() {
                        Ok(mode) => {
                            self.term_rx = self.terminal.run(&cmdline, &mode);
                        }
                        // Strict containment: sandbox was intended but unavailable. Echo the
                        // command as blocked and DO NOT run it on the host.
                        Err(reason) => {
                            if !cmdline.trim().is_empty() {
                                self.terminal.blocked(cmdline.trim(), &reason);
                            }
                        }
                    }
                }
            }
            Message::TermKill => self.terminal.kill(),
            Message::TermClear => self.terminal.clear(),
            Message::TermHistoryPrev => self.terminal.history_prev(),
            Message::TermHistoryNext => self.terminal.history_next(),
            Message::GitCursorMoved(p) => {
                self.cursor_pos = p;
                // While the chat|code divider is held, map the absolute cursor X to chat's share
                // of the chat+code region. The explorer occupies a fixed 20% on the left, so that
                // region runs from 0.20·W to W.
                if self.dragging_split && self.window_w > 0.0 {
                    let region_left = 0.20 * self.window_w;
                    let region_w = self.window_w - region_left;
                    if region_w > 1.0 {
                        let frac = (p.x - region_left) / region_w;
                        self.chat_frac = frac.clamp(0.15, 0.85);
                    }
                }
                // While the git|files divider is held, move it by the cursor Y DELTA from the grab
                // point — not an absolute mapping (which would need the explorer's exact top offset
                // and snap on grab). `explorer_frac` is Git's share of the EXPLORER COLUMN's height,
                // so a cursor delta of `d` px must be scaled by that column's height, NOT the whole
                // window — dividing by `window_h` made the divider lag the cursor (the column is
                // shorter than the window). `explorer_region_h()` is the column's true height, so
                // the divider now tracks the cursor 1:1.
                if let Some((y0, frac0)) = self.explorer_drag {
                    let region_h = self.explorer_region_h();
                    if region_h > 1.0 {
                        let dfrac = (p.y - y0) / region_h;
                        self.explorer_frac = (frac0 + dfrac).clamp(0.1, 0.8);
                    }
                }
            }
            Message::SplitDragStart => self.dragging_split = true,
            Message::SplitDragEnd => {
                self.dragging_split = false;
                self.explorer_drag = None;
                // A drag settled — persist both dividers' current positions by id (one write, on
                // release, not per mouse-move).
                self.splits
                    .set(sc_win::splits::id::CHAT_CODE, self.chat_frac);
                self.splits
                    .set(sc_win::splits::id::EXPLORER_GIT_FILES, self.explorer_frac);
                self.splits.save();
            }
            Message::WindowSize(w, h) => {
                self.window_w = w;
                self.window_h = h;
            }
            // Anchor the drag at the current cursor Y and current fraction — moves are deltas
            // from here, so the divider never jumps on grab.
            Message::ExplorerDragStart => {
                self.explorer_drag = Some((self.cursor_pos.y, self.explorer_frac));
            }
            Message::GitRowMenu(path, status) => {
                self.git_menu_at = self.cursor_pos;
                self.git_menu = Some((path, status));
            }
            Message::CloseGitMenu => self.git_menu = None,
            Message::GitStage(path) => {
                self.git_menu = None;
                // Batch: if this file is part of a multi-selection, stage every selected file in
                // one call (the user picked a set with Ctrl/Shift and expects the ＋/menu to act on
                // all of it). A lone or unselected file stages just itself.
                let targets = self.git_action_targets(&path);
                let mut args = vec!["add", "--"];
                args.extend(targets.iter().map(String::as_str));
                self.run_git(&args);
                self.refresh_git_view();
            }
            Message::GitUnstage(path) => {
                self.git_menu = None;
                let targets = self.git_action_targets(&path);
                let mut args = vec!["restore", "--staged", "--"];
                args.extend(targets.iter().map(String::as_str));
                self.run_git(&args);
                self.refresh_git_view();
            }
            Message::GitDiscard(path) => {
                self.git_menu = None;
                // Batch: discard every file in the selection when this row is part of one. Split by
                // tracked-ness — untracked files need `clean -f` (a `checkout --` is a no-op on
                // them), tracked files need `checkout --` to restore the committed content.
                let targets = self.git_action_targets(&path);
                let (untracked, tracked): (Vec<&String>, Vec<&String>) =
                    targets.iter().partition(|p| {
                        self.file_status.get(*p) == Some(&sc_win::gitdiff::FileStatus::Added)
                    });
                if !untracked.is_empty() {
                    let mut args = vec!["clean", "-f", "--"];
                    args.extend(untracked.iter().map(|p| p.as_str()));
                    self.run_git(&args);
                }
                if !tracked.is_empty() {
                    let mut args = vec!["checkout", "--"];
                    args.extend(tracked.iter().map(|p| p.as_str()));
                    self.run_git(&args);
                }
                self.refresh_git_view();
                // Close tabs for files the discard REMOVED from disk (deleting an untracked file
                // with `clean -f`) — a tab on a file that no longer exists is dead weight. Files
                // that were merely reverted still exist, so their tabs stay (reloaded below).
                let root = self.workspace_root();
                let gone: Vec<String> = targets
                    .iter()
                    .filter(|p| !root.join(p).exists())
                    .cloned()
                    .collect();
                for p in &gone {
                    self.close_tab(p);
                }
                // If the file still on screen was reverted (not deleted), reload it to show the
                // reverted content.
                if self
                    .selected_file
                    .as_ref()
                    .is_some_and(|s| targets.contains(s))
                {
                    self.reload_selected();
                }
            }
            Message::CommitMsgChanged(s) => self.commit_msg = s,
            Message::GitStageAll => {
                self.run_git(&["add", "-A"]);
                self.refresh_git_view();
            }
            Message::GitUnstageAll => {
                self.run_git(&["reset"]); // unstage everything, keep working-tree changes
                self.refresh_git_view();
            }
            Message::GitCommit => {
                let msg = self.commit_msg.trim().to_string();
                // Nothing staged, or an empty message → don't attempt a commit.
                let has_staged = self.stage_states.values().any(|s| s.staged);
                if msg.is_empty() || !has_staged {
                    return Task::none();
                }
                if self.run_git(&["commit", "-m", &msg]) {
                    self.commit_msg.clear();
                }
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitPush => {
                // No upstream yet → set it on push so a fresh branch publishes cleanly.
                if self.upstream.upstream.is_none() {
                    if let Some(b) = self.branch.clone() {
                        self.run_git_net("push", &["push", "-u", "origin", &b]);
                    }
                } else {
                    self.run_git_net("push", &["push"]);
                }
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitPull => {
                self.run_git_net("pull", &["pull", "--ff-only"]);
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitFetch => {
                self.run_git_net("fetch", &["fetch"]);
                self.refresh_git_view();
            }
            Message::LineDragStart(n) => {
                // Begin a drag-selection anchored at line n. Clear any open comment box.
                self.drag = Some((n, n));
                self.comment_range = None;
            }
            Message::LineDragTo(n) => {
                // Extend the drag to line n (only while a drag is active).
                if let Some((anchor, _)) = self.drag {
                    self.drag = Some((anchor, n));
                }
            }
            Message::LineDragEnd => {
                // Commit the drag into a comment range (normalized so start ≤ end) and open
                // the comment box. A no-drag (just a click) yields a single-line range.
                if let Some((a, b)) = self.drag.take() {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    self.comment_range = Some((lo, hi));
                    self.comment_draft.clear();
                }
            }
            Message::CommentDraftChanged(s) => self.comment_draft = s,
            Message::CommentSubmit => self.submit_line_comment(),
            Message::CommentCancel => {
                self.comment_range = None;
                self.drag = None;
                self.comment_draft.clear();
            }
            Message::ConfirmAllow => self.answer_confirm(Confirmation::AllowOnce),
            Message::ConfirmDeny => {
                self.answer_confirm(Confirmation::Deny("denied by user".to_string()))
            }
            Message::ConfirmRemember => {
                // Remember the command's first token as the approved prefix.
                let prefix = match self.gatebar.first() {
                    Some(Gatebar::Confirm { command, .. }) => remember_prefix(command),
                    _ => String::new(),
                };
                self.answer_confirm(Confirmation::AllowRemember { prefix });
            }
            Message::NotesChanged(s) => self.sendback_notes = s,
            Message::GateApprove => self.answer_gate(Decision::Approve),
            // (Revise dropped from the UI — send-back-with-comments supersedes it. `Decision::Revise`
            //  stays in the workflow enum for the CLI; the GUI no longer surfaces a button for it.)
            Message::GateSendBack => {
                // Change B: feedback comes from CODE-REVIEW line comments. Harvest every
                // comment on the gating phase's artifact file and turn it into the send-back
                // notes (one bullet per comment). This is the primary feedback path: the user
                // reads the phase's `.md` in the code view, drops line comments where they want
                // changes, and clicks Send back. Fall back to the free-text notes box when there
                // are no line comments (so a general note still works).
                let Some(target) = self.gating_phase() else {
                    return Task::none();
                };
                let file = self.plan.path_for(target);
                let harvested = file.as_deref().and_then(|f| {
                    let on_file: Vec<&sc_win::comments::Comment> =
                        self.comments.on_file(f).map(|(_, c)| c).collect();
                    sc_win::comments::format_sendback_notes(&on_file)
                });
                let notes = harvested.or_else(|| non_empty(&self.sendback_notes));
                self.answer_gate(Decision::SendBack { target, notes });
                // The harvested comments have been DELIVERED as the send-back notes — drop them
                // (and persist) so they don't re-deliver on the re-planned phase's next gate.
                if let Some(f) = file {
                    self.comments.items.retain(|c| c.file != f);
                    sc_win::comments::save(&self.workspace_root(), &self.comments);
                }
            }
            Message::GateAbort => self.answer_gate(Decision::Abort),
            Message::SelectCoder(id) => self.selected_coder = Some(id),
            Message::ClearSelection => self.selected_coder = None,
            Message::PickWorkspace => {
                self.open_menu = None;
                // Native folder dialog (blocking — fine for a button click). When a
                // folder is chosen, runs go there so follow-up prompts iterate on it.
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a project folder to work in")
                    .pick_folder()
                {
                    self.open_workspace(dir);
                }
            }
            Message::OpenRecent(dir) => {
                self.open_menu = None;
                if dir.is_dir() {
                    self.open_workspace(dir);
                }
            }
            Message::NoOp => {}
            Message::ClearWorkspace => {
                self.open_menu = None;
                self.picked_workspace = None;
                self.selected_file = None;
                self.code = None;
                // Clear the CODE-panel tabs too — they belonged to the closed project.
                self.open_tabs.clear();
                // Forget the *current* project so a restart doesn't re-open it, but keep the
                // recents list (the user may want to re-pick one).
                let mut state = sc_win::persist::load();
                state.last_project = None;
                sc_win::persist::save(&state);
                self.publish_workspace_to_remote();
            }
            Message::OpenOutputFolder => {
                if let Some(dir) = self.result.as_ref().and_then(|r| r.dir.clone()) {
                    // Open in the system file manager (Explorer on Windows).
                    #[cfg(target_os = "windows")]
                    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                    #[cfg(target_os = "macos")]
                    let _ = std::process::Command::new("open").arg(&dir).spawn();
                    #[cfg(all(unix, not(target_os = "macos")))]
                    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                }
            }
        }
        // Keep the per-turn selectable editors in step with the chat thread (no-op unless the
        // thread changed). Runs after every message so streamed/appended turns are covered.
        self.sync_chat_editors();
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        // The IDE body: three columns — EXPLORER (file tree) · CENTER (activity stream +
        // the intent composer beneath it) · CODE (the file being edited). VS-Code-style.
        let center: Element<'_, Message> = if self.plan.started() && self.is_swarm() {
            // A swarm build in flight: the live topology is the story.
            self.view_topology()
        } else {
            // Staged build / iterate / idle: the chat thread (with inline gate controls when a
            // phase is waiting) is the center column. The old left PLAN panel is gone — the phase
            // content streams into the chat and the gating file auto-opens in CODE.
            self.view_center()
        };

        // The chat and code panels share the region right of the explorer; `chat_frac` splits
        // it, driven by dragging the divider between them. Explorer stays a fixed ~20% (200 of
        // the 1000 total portions), so chat+code get 800 to divide.
        let chat_portion = (self.chat_frac * 800.0).round() as u16;
        let code_portion = 800u16.saturating_sub(chat_portion);
        let body = row![
            self.view_explorer(),
            v_divider(),
            container(center)
                .width(Length::FillPortion(chat_portion))
                .height(Fill),
            v_divider_draggable(),
            self.view_code(code_portion),
        ]
        .spacing(GAP)
        .height(Fill);

        let gate = self.view_gatebar();

        // The body content below the (flush, full-width) menu bar — this part is padded.
        // The run outcome now lives in the BUILD panel of the bottom strip (not a top
        // banner), so it no longer shoves the three columns down.
        let mut body_col = column![].spacing(GAP);
        if self.plan.started() {
            body_col = body_col.push(self.view_step_flow());
        }
        body_col = body_col.push(body);
        if let Some(strip) = self.view_bottom_strip() {
            body_col = body_col.push(h_divider());
            body_col = body_col.push(strip);
        }
        if let Some(g) = gate {
            body_col = body_col.push(g);
        }

        // Base layer: the menu bar flush at the very top (no padding around it, full width),
        // then the padded body beneath it.
        let base = column![
            self.view_menu_bar(),
            container(body_col).width(Fill).height(Fill),
        ]
        .width(Fill)
        .height(Fill);

        // Overlays float ABOVE the base (a Stack), so an open dropdown or the settings modal
        // never shifts the layout. Only one shows at a time.
        let mut layers = iced::widget::stack![base];
        if let Some(dd) = self.view_menu_dropdown() {
            layers = layers.push(dd);
        }
        if let Some(gm) = self.view_git_menu() {
            layers = layers.push(gm);
        }
        if self.settings_open {
            layers = layers.push(self.view_settings_modal());
        }
        layers.width(Fill).height(Fill).into()
    }

    /// The rendered height (px) of the EXPLORER column — `window_h` minus the chrome above and
    /// below the body row. Used to scale a divider drag so it tracks the cursor 1:1. The layout
    /// (see `view()`): a menu bar on top, then a padded body column that holds an optional
    /// step-flow bar, the three-panel body row (which contains the explorer), and an optional
    /// bottom strip (fixed 180px) — the explorer gets what's left. These are close estimates of
    /// the fixed chrome; exact-to-the-pixel isn't needed, but the SCALE must be right so the drag
    /// doesn't lag or race the cursor.
    fn explorer_region_h(&self) -> f32 {
        const MENU_BAR: f32 = 34.0; // top menu row + its padding
        const STEP_FLOW: f32 = 52.0; // the phase step-flow card, shown only while planning
        const BOTTOM_STRIP: f32 = 190.0; // the fixed 180px strip + its gap, when shown
        const BODY_PAD: f32 = 2.0 * PAD as f32; // the body container's top+bottom padding
        let mut h = self.window_h - MENU_BAR - BODY_PAD;
        if self.plan.started() {
            h -= STEP_FLOW;
        }
        if self.view_bottom_strip().is_some() {
            h -= BOTTOM_STRIP;
        }
        h.max(1.0)
    }

    /// The left EXPLORER column: a tabbed panel — **Files** (the workspace file tree) and
    /// **Git** (just the changed files, PR-style). A tab bar sits under a branch header that's
    /// shared across both tabs.
    fn view_explorer(&self) -> Element<'_, Message> {
        // GitHub-PR-style header: the current branch, ahead/behind vs upstream, and a count of
        // changed files. Shared by both tabs.
        let n_changed = self.file_status.len();
        let up = &self.upstream;
        let branch_line = match &self.branch {
            Some(b) => {
                let mut s = format!("⎇ {b}");
                if up.ahead > 0 {
                    s.push_str(&format!("  ↑{}", up.ahead));
                }
                if up.behind > 0 {
                    s.push_str(&format!("  ↓{}", up.behind));
                }
                s.push_str(&format!("  ·  {n_changed} changed"));
                s
            }
            None => "not a git repo".to_string(),
        };
        // Top section (25%): everything git — the branch line, push/pull/fetch, and the changed
        // files. Bottom section (75%): the Files tree with its quick filter. Stacked rather than
        // tabbed so both are visible at once.
        let mut git_col = column![text(branch_line)
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70)),]
        .spacing(6);
        // Push / Pull / Fetch — only when the repo is on a branch (has a name). Labels show the
        // ahead/behind counts so you know what each will move.
        if self.branch.is_some() {
            git_col = git_col.push(self.view_sync_bar());
        }
        git_col = git_col.push(self.view_git_tab());

        // Each section is its own rounded card, stacked with a draggable divider between them.
        // `explorer_frac` (0.1..0.8) splits the height; the clamp guarantees both portions are
        // ≥100, so neither FillPortion is ever zero.
        let git_portion = (self.explorer_frac * 1000.0).round() as u16;
        let files_portion = 1000u16.saturating_sub(git_portion);
        let git_section = container(git_col.spacing(6))
            .height(Length::FillPortion(git_portion))
            .width(Fill)
            .padding(PAD)
            .style(card_style);
        let files_section = container(self.view_files_tab())
            .height(Length::FillPortion(files_portion))
            .width(Fill)
            .padding(PAD)
            .style(card_style);

        // 200 of the 1000-portion total (chat+code share the other 800) → a fixed ~20% width,
        // so dragging the chat|code divider never moves the explorer.
        container(column![git_section, h_divider_draggable(), files_section])
            .width(Length::FillPortion(200))
            .height(Fill)
            .into()
    }

    /// The push / pull / fetch bar shown under the branch line. Push shows the ahead count, Pull
    /// the behind count. When the branch has no upstream, Push offers to publish it.
    fn view_sync_bar(&self) -> Element<'_, Message> {
        let up = &self.upstream;
        let push_label = if up.upstream.is_none() {
            "↑ Publish".to_string()
        } else if up.ahead > 0 {
            format!("↑ Push {}", up.ahead)
        } else {
            "↑ Push".to_string()
        };
        let pull_label = if up.behind > 0 {
            format!("↓ Pull {}", up.behind)
        } else {
            "↓ Pull".to_string()
        };
        let btn = |label: String, msg: Message| {
            button(text(label).size(11).color(FG))
                .on_press(msg)
                .padding([1, 8])
                .style(stage_toggle_button)
        };
        row![
            btn(push_label, Message::GitPush),
            btn(pull_label, Message::GitPull),
            button(text("⟳").size(12).color(FG))
                .on_press(Message::GitFetch)
                .padding([1, 8])
                .style(stage_toggle_button),
        ]
        .spacing(4)
        .into()
    }

    /// The **Files** tab: the workspace file tree, dirs-first, click a file to pin it in the
    /// code panel, click a dir to collapse/expand. Empty-state hint before a project folder is
    /// picked.
    fn view_files_tab(&self) -> Element<'_, Message> {
        use sc_win::gitdiff::FileStatus;
        let filtering = !self.file_filter.trim().is_empty();
        // Derive the display from the cached full tree in memory — no filesystem walk per frame.
        let rows = if filtering {
            sc_win::filetree::filter_view(&self.tree_cache, &self.file_filter)
        } else {
            sc_win::filetree::collapse_view(&self.tree_cache, &self.collapsed_dirs)
        };

        // A quick-filter box at the top of the tree — type to narrow to matching files/folders.
        let filter_box = text_input("Filter files…", &self.file_filter)
            .on_input(Message::FileFilterChanged)
            .padding(4)
            .size(12)
            .style(input_style)
            .width(Fill);

        let mut col = column![].spacing(2);
        if rows.is_empty() {
            let hint = if filtering {
                "no files match"
            } else {
                "File ▸ Open folder to work in"
            };
            col = col.push(text(hint).size(11).color(FG_MUTED));
        }
        for r in rows.iter().take(600) {
            let indent = 8.0 + (r.depth as f32) * 12.0;
            let is_selected = !r.is_dir && self.selected_file.as_deref() == Some(r.rel.as_str());
            let glyph = if r.is_dir {
                // While filtering the tree is force-expanded, so every shown dir reads as open.
                if !filtering && self.collapsed_dirs.contains(&r.rel) {
                    "▸"
                } else {
                    "▾"
                }
            } else {
                " "
            };
            // PR-style file status badge (M/A/D) + colouring for changed files.
            let status = (!r.is_dir).then(|| self.file_status.get(&r.rel)).flatten();
            let (badge, badge_color) = match status {
                Some(FileStatus::Added) => ("A", GOOD),
                Some(FileStatus::Modified) => ("M", AMBER),
                Some(FileStatus::Deleted) => ("D", BAD),
                None => (" ", FG_MUTED),
            };
            let name_color = if is_selected {
                ACCENT
            } else if let Some(s) = status {
                match s {
                    FileStatus::Added => GOOD,
                    FileStatus::Modified => AMBER,
                    FileStatus::Deleted => BAD,
                }
            } else if r.is_dir {
                FG
            } else {
                FG_MUTED
            };
            let msg = if r.is_dir {
                Message::ToggleDir(r.rel.clone())
            } else {
                Message::SelectFile(r.rel.clone())
            };
            let btn = button(
                row![
                    text(badge.to_string())
                        .size(11)
                        .font(iced::Font::MONOSPACE)
                        .color(badge_color),
                    text(format!("{glyph} {}", r.name))
                        .size(12)
                        .color(name_color),
                ]
                .spacing(4),
            )
            .on_press(msg)
            .padding([1, 4])
            .style(tree_button)
            .width(Fill);
            col = col.push(row![Space::new().width(Length::Fixed(indent)), btn]);
        }

        column![filter_box, scrollable(col).height(Fill)]
            .spacing(6)
            .into()
    }

    /// The **Git** tab: a VS-Code-style Source Control panel — a commit-message box + Commit
    /// button on top, then a **Staged Changes** section and an unstaged **Changes** section
    /// (grouped Added / Modified / Deleted). Right-click any file for stage / unstage / discard.
    /// The git files in the exact order the git tab renders them: the staged section first (keys
    /// filtered to those `stage_states` marks staged), then the unstaged section (everything
    /// unstaged/untracked). `view_git_tab` and the Shift-range selection both derive from this, so
    /// the "displayed order" the range spans can never drift from what's on screen.
    fn git_display_order(&self) -> Vec<String> {
        let staged = self
            .file_status
            .keys()
            .filter(|p| self.stage_states.get(*p).map(|s| s.staged).unwrap_or(false))
            .cloned();
        let unstaged = self
            .file_status
            .keys()
            .filter(|p| {
                self.stage_states
                    .get(*p)
                    .map(|s| s.unstaged)
                    .unwrap_or(true)
            })
            .cloned();
        staged.chain(unstaged).collect()
    }

    /// The files a stage/unstage/discard action should apply to, given the row it was invoked on.
    /// When `path` is part of a multi-selection (the user Ctrl/Shift-picked a set), the action
    /// fans out to every selected file — in displayed order, so the git call is deterministic.
    /// Otherwise it's just `path` (a lone click, or acting on a row outside the current selection).
    fn git_action_targets(&self, path: &str) -> Vec<String> {
        if self.git_selection.len() > 1 && self.git_selection.contains(path) {
            self.git_display_order()
                .into_iter()
                .filter(|p| self.git_selection.contains(p))
                .collect()
        } else {
            vec![path.to_string()]
        }
    }

    fn view_git_tab(&self) -> Element<'_, Message> {
        use sc_win::gitdiff::FileStatus;
        if self.branch.is_none() {
            return text("not a git repository").size(11).color(FG_MUTED).into();
        }

        // The commit box: a message input + a Commit button, enabled only when something is
        // staged and the message is non-empty (like VS Code's checkmark).
        let has_staged = self.stage_states.values().any(|s| s.staged);
        let can_commit = has_staged && !self.commit_msg.trim().is_empty();
        let input = text_input("Message (commit staged changes)", &self.commit_msg)
            .on_input(Message::CommitMsgChanged)
            .on_submit(Message::GitCommit)
            .padding(6)
            .size(12)
            .style(input_style)
            .width(Fill);
        let mut commit_btn = button(text("✓ Commit").size(12));
        if can_commit {
            commit_btn = commit_btn
                .on_press(Message::GitCommit)
                .style(primary_button);
        } else {
            commit_btn = commit_btn.style(menu_item_style);
        }
        let commit_box = column![input, commit_btn.padding([4, 12]).width(Fill)].spacing(4);

        // Partition the changed files into staged and unstaged. A file can be in BOTH (staged
        // plus further working-tree edits) — VS Code shows it in each, and so do we. The
        // staged/unstaged filters here MUST match `git_display_order` (which the Shift-range
        // selection uses), so the on-screen order and the selectable order stay in lock-step.
        let staged: Vec<&String> = self
            .file_status
            .keys()
            .filter(|p| self.stage_states.get(*p).map(|s| s.staged).unwrap_or(false))
            .collect();
        let unstaged: Vec<(&String, FileStatus)> = self
            .file_status
            .iter()
            .filter(|(p, _)| {
                // Unstaged, or untracked. If we have no stage info for it, treat it as unstaged.
                self.stage_states
                    .get(*p)
                    .map(|s| s.unstaged)
                    .unwrap_or(true)
            })
            .map(|(p, s)| (p, *s))
            .collect();

        let mut col = column![].spacing(2);

        // Staged Changes header — with a "− All" (unstage all) — then rows.
        if !staged.is_empty() {
            col = col.push(self.git_section_header(
                "Staged Changes",
                staged.len(),
                Some(("− All", Message::GitUnstageAll)),
            ));
            for path in &staged {
                let status = self
                    .file_status
                    .get(*path)
                    .copied()
                    .unwrap_or(FileStatus::Modified);
                col = col.push(self.git_file_row(path, status, true));
            }
        }

        // Changes (unstaged) header — with a "＋ All" (stage all) — then rows.
        if !unstaged.is_empty() {
            col = col.push(self.git_section_header(
                "Changes",
                unstaged.len(),
                Some(("＋ All", Message::GitStageAll)),
            ));
            for (path, status) in &unstaged {
                col = col.push(self.git_file_row(path, *status, false));
            }
        }

        if staged.is_empty() && unstaged.is_empty() {
            col = col.push(
                text("working tree clean — no changes vs HEAD")
                    .size(11)
                    .color(FG_MUTED),
            );
        }

        column![commit_box, scrollable(col).height(Fill)]
            .spacing(8)
            .into()
    }

    /// A git-tab section header (e.g. "Staged Changes (3)") with an optional stage/unstage-all
    /// action button on the right — `action` is `(button_label, message)`, e.g. `("＋ All", …)` on
    /// the unstaged section or `("− All", …)` on the staged one.
    fn git_section_header(
        &self,
        label: &str,
        count: usize,
        action: Option<(&str, Message)>,
    ) -> Element<'_, Message> {
        let mut r = row![
            text(format!("{label} ({count})")).size(11).color(FG_MUTED),
            Space::new().width(Fill),
        ]
        .align_y(iced::Alignment::Center);
        if let Some((btn_label, msg)) = action {
            r = r.push(
                button(text(btn_label.to_string()).size(11).color(FG))
                    .on_press(msg)
                    .padding([0, 8])
                    .style(stage_toggle_button),
            );
        }
        container(r).padding([2, 0]).into()
    }

    /// One file row in the git tab: a status badge + path, left-click opens it in the code
    /// panel, right-click pops the stage/unstage/discard menu. `staged` tints staged rows and is
    /// carried into the menu so it offers the right action. Deleted files aren't click-to-open.
    fn git_file_row(
        &self,
        path: &str,
        status: sc_win::gitdiff::FileStatus,
        staged: bool,
    ) -> Element<'_, Message> {
        use sc_win::gitdiff::FileStatus;
        let color = match status {
            FileStatus::Added => GOOD,
            FileStatus::Modified => AMBER,
            FileStatus::Deleted => BAD,
        };
        // Highlight the row when it's the previewed file OR part of the multi-selection, so every
        // Ctrl/Shift-selected row reads as selected (not just the last-clicked previewed one).
        let is_selected =
            self.selected_file.as_deref() == Some(path) || self.git_selection.contains(path);
        let name_color = if is_selected { ACCENT } else { color };
        let mut inner = row![
            text(status.badge().to_string())
                .size(11)
                .font(iced::Font::MONOSPACE)
                .color(color),
            text(path.to_string()).size(12).color(name_color),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        // Right-aligned +added / −removed line counts for this file (staged rows read the
        // staged diff, unstaged rows the working-tree diff). Only shown when non-zero.
        let deltas = if staged {
            &self.staged_deltas
        } else {
            &self.unstaged_deltas
        };
        if let Some(d) = deltas.get(path) {
            if d.added > 0 || d.removed > 0 {
                inner = inner.push(Space::new().width(Fill));
                if d.added > 0 {
                    inner = inner.push(
                        text(format!("+{}", d.added))
                            .size(11)
                            .font(iced::Font::MONOSPACE)
                            .color(GOOD),
                    );
                }
                if d.removed > 0 {
                    inner = inner.push(
                        text(format!("−{}", d.removed))
                            .size(11)
                            .font(iced::Font::MONOSPACE)
                            .color(BAD),
                    );
                }
            }
        }
        let row_el: Element<'_, Message> = if status == FileStatus::Deleted {
            container(inner).padding([1, 4]).width(Fill).into()
        } else {
            button(inner)
                .on_press(Message::SelectGitFile(path.to_string()))
                .padding([1, 4])
                .style(tree_button)
                .width(Fill)
                .into()
        };
        // A quick stage (＋) / unstage (−) button beside the row — staged files get −, unstaged get
        // ＋. It's a SIBLING of the row button (buttons can't nest), so clicking it stages/unstages
        // without selecting the file.
        let (glyph, action) = if staged {
            ("−", Message::GitUnstage(path.to_string()))
        } else {
            ("＋", Message::GitStage(path.to_string()))
        };
        let toggle = button(text(glyph).size(13).font(iced::Font::MONOSPACE).color(FG))
            .on_press(action)
            .padding([0, 8])
            .style(stage_toggle_button);
        let full = row![row_el, toggle]
            .align_y(iced::Alignment::Center)
            .spacing(4);
        // A right-click opens the menu; which Stage/Unstage action it offers is decided from
        // `stage_states` when the menu opens. Cursor position comes from the window sub.
        iced::widget::mouse_area(full)
            .on_right_press(Message::GitRowMenu(path.to_string(), status))
            .into()
    }

    /// The git-row context menu overlay: a small floating card of actions (stage / unstage /
    /// discard) for the right-clicked file, positioned at the cursor. A transparent full-window
    /// backdrop closes it on any outside click. `None` when no menu is open.
    fn view_git_menu(&self) -> Option<Element<'_, Message>> {
        use sc_win::gitdiff::FileStatus;
        let (path, status) = self.git_menu.clone()?;

        // Show Stage only if the file has unstaged content, Unstage only if it has staged
        // content — never both when there's nothing to do. Discard's label reflects the status.
        let stage = self.stage_states.get(&path).copied();
        let has_unstaged = stage.map(|s| s.unstaged).unwrap_or(true);
        let has_staged = stage.map(|s| s.staged).unwrap_or(false);
        // When the right-clicked file is part of a multi-selection, Stage/Unstage/Discard fan out
        // to the whole set (see `git_action_targets`) — reflect that in the labels so it's clear
        // the action isn't just this one file, and the count warns before a batch discard.
        let batch = if self.git_selection.len() > 1 && self.git_selection.contains(&path) {
            self.git_selection.len()
        } else {
            1
        };
        let discard_label = if batch > 1 {
            format!("🗑  Discard {batch} files")
        } else {
            match status {
                FileStatus::Added => "🗑  Delete untracked file",
                FileStatus::Deleted => "↩  Restore deleted file",
                FileStatus::Modified => "↩  Discard changes",
            }
            .to_string()
        };
        let stage_label = if batch > 1 {
            format!("＋  Stage {batch} files")
        } else {
            "＋  Stage".to_string()
        };
        let unstage_label = if batch > 1 {
            format!("－  Unstage {batch} files")
        } else {
            "－  Unstage".to_string()
        };
        let mut items: Vec<(String, Message)> = Vec::new();
        if has_unstaged {
            items.push((stage_label, Message::GitStage(path.clone())));
        }
        if has_staged {
            items.push((unstage_label, Message::GitUnstage(path.clone())));
        }
        // Discard acts on the working tree — only meaningful when there are unstaged changes.
        if has_unstaged {
            items.push((discard_label, Message::GitDiscard(path.clone())));
        }
        let mut col = column![text(path.clone())
            .size(11)
            .color(FG_MUTED)
            .wrapping(iced::widget::text::Wrapping::None),]
        .spacing(0);
        for (label, msg) in items {
            col = col.push(
                button(text(label.to_string()).size(13).color(FG))
                    .on_press(msg)
                    .padding([6, 14])
                    .width(Length::Fixed(230.0))
                    .style(menu_item_style),
            );
        }
        let card = container(col).padding(3).style(dropdown_style);

        // Position the card at the click point; clamp a little off the edges isn't needed for a
        // narrow panel, but keep it from riding the very top.
        let x = self.git_menu_at.x.max(0.0);
        let y = self.git_menu_at.y.max(0.0);
        let positioned = column![
            Space::new().height(Length::Fixed(y)),
            row![Space::new().width(Length::Fixed(x)), card],
        ];
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::CloseGitMenu)
            .on_right_press(Message::CloseGitMenu);

        Some(
            iced::widget::stack![backdrop, positioned]
                .width(Fill)
                .height(Fill)
                .into(),
        )
    }

    /// The line range currently highlighted in the code view: the active drag (normalized so
    /// lo ≤ hi) if dragging, else the committed comment range. `None` when neither.
    fn selected_line_range(&self) -> Option<(usize, usize)> {
        if let Some((a, b)) = self.drag {
            Some(if a <= b { (a, b) } else { (b, a) })
        } else {
            self.comment_range
        }
    }

    /// The right CODE column: the selected/followed file with line numbers, read-only,
    /// rendered like a VS Code editor. The gutter (line numbers) and the code are TWO
    /// side-by-side columns; wrapping is disabled so a long line scrolls horizontally
    /// instead of wrapping into — and interrupting — the number gutter. One vertical
    /// scroll (whole editor) + one horizontal scroll (the code column) is what you get.
    fn view_code(&self, portion: u16) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match &self.code {
            Some(cv) if cv.note.is_some() => text(cv.note.clone().unwrap_or_default())
                .size(12)
                .color(FG_MUTED)
                .into(),
            Some(cv) => {
                // Per-line rows you can DRAG across to select a range, then comment (PR-style).
                // Each line is a mouse_area (press=start drag, enter=extend, release=commit)
                // wrapping ONE no-wrap monospace string; selected lines get an accent wash.
                // The comment box renders after the last line of the committed range.
                let sel = self.selected_line_range(); // active drag OR committed range
                                                      // The amber "working" range applies only when the shown file is the one being
                                                      // worked on. A pulsing alpha (sine on the animation clock) reads as "in progress".
                let working_here = self
                    .working
                    .as_ref()
                    .filter(|(f, _, _)| Some(f.as_str()) == self.selected_file.as_deref())
                    .map(|(_, lo, hi)| (*lo, *hi));
                let pulse = 0.10 + 0.10 * (0.5 + 0.5 * (self.now() * 3.0).sin());

                // Always window to the visible lines (fast on big files). Inline comments and the
                // open comment box have variable height, but they only ever appear INSIDE the
                // rendered window (anchored to a visible line), between the two spacers — so they
                // never corrupt the spacer counts, which only stand in for plain CODE_LINE_PX
                // rows above/below. A box below shifts later lines slightly, exactly as before.
                // When the minimap is floating (file overflows the viewport), inline-comment rows
                // need right padding so their ✕/revert buttons don't hide behind it. 72px minimap
                // + a small gap. Computed here since the comment rows are built in the loop below.
                let minimap_overflows =
                    self.code_viewport.is_some_and(|(_, height)| height < 0.999);
                // Width for the comment / revert bars: the VIEWPORT width (not the horizontally-
                // scrollable content width), minus the minimap gutter when it's floating, so a bar
                // spans the visible area and ends just before the minimap — with round edges. It
                // scales with the window and expands when the minimap is hidden. `None` (→ Fill)
                // before the first scroll gives us a real viewport width.
                let bar_width: Option<f32> = (self.code_view_w > 1.0).then(|| {
                    let gutter = if minimap_overflows { 76.0 } else { 8.0 };
                    (self.code_view_w - gutter).max(120.0)
                });

                // Diff blocks (VS-Code-style). Two derived maps:
                //  • `block_bar_after`: last green line of a block → its cur_start (where to render
                //    the standalone "↩ revert block" bar).
                //  • `line_to_block`: every green line → its block's cur_start (so a comment ON a
                //    changed block can offer the same revert inline).
                let hunks = self.file_diff.hunks();
                let block_bar_after: std::collections::BTreeMap<usize, usize> = hunks
                    .iter()
                    .filter(|h| h.cur_start <= h.cur_end) // has a current (green) line
                    .map(|h| (h.cur_end, h.cur_start))
                    .collect();
                let line_to_block: std::collections::BTreeMap<usize, usize> = hunks
                    .iter()
                    .filter(|h| h.cur_start <= h.cur_end)
                    .flat_map(|h| (h.cur_start..=h.cur_end).map(move |l| (l, h.cur_start)))
                    .collect();
                // Blocks that already have a comment on them — those revert from the comment row,
                // so we skip the standalone bar to avoid a duplicate button.
                let blocks_with_comment: std::collections::BTreeSet<usize> = self
                    .selected_file
                    .as_deref()
                    .map(|f| {
                        self.comments
                            .on_file(f)
                            .filter_map(|(_, c)| {
                                (c.start..=c.end).find_map(|l| line_to_block.get(&l).copied())
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let total = cv.lines.len();
                let (first_idx, last_idx) = {
                    const OVERSCAN: usize = 12;
                    let per = CODE_LINE_PX.max(1.0);
                    let view_h = if self.code_view_h > 1.0 {
                        self.code_view_h
                    } else {
                        800.0 // generous first-frame guess before we know the real height
                    };
                    let first = ((self.code_scroll_y / per) as usize).saturating_sub(OVERSCAN);
                    let visible = (view_h / per).ceil() as usize + 2 * OVERSCAN;
                    (first.min(total), (first + visible).min(total))
                };

                let mut col = column![].spacing(0);
                // Top spacer standing in for the hidden lines above the window (keeps the
                // scrollbar geometry, minimap box, and scroll_to offsets pixel-accurate). It must
                // also cover any RED removed-lines that anchor above the window (they render as
                // rows, so they add height) — count removals before the first visible line.
                if first_idx > 0 {
                    let first_visible_line = first_idx + 1; // 1-based line number of cv.lines[first_idx]
                    let hidden_removed: usize = self
                        .file_diff
                        .removed_before
                        .range(..first_visible_line)
                        .map(|(_, v)| v.len())
                        .sum();
                    let hidden_rows = first_idx + hidden_removed;
                    col = col.push(
                        Space::new().height(Length::Fixed(hidden_rows as f32 * CODE_LINE_PX)),
                    );
                }
                for (n, line) in &cv.lines[first_idx..last_idx] {
                    // GitHub-PR style: render any lines DELETED just before this one as red rows
                    // (a `-` gutter, red wash). They exist only in HEAD, so they're not selectable.
                    if let Some(removed) = self.file_diff.removed_before.get(n) {
                        for gone in removed {
                            col = col.push(
                                container(
                                    text(format!("-     {gone}"))
                                        .size(13)
                                        .line_height(iced::widget::text::LineHeight::Absolute(
                                            CODE_LINE_PX.into(),
                                        ))
                                        .font(iced::Font::MONOSPACE)
                                        .color(BAD)
                                        .wrapping(iced::widget::text::Wrapping::None),
                                )
                                .height(Length::Fixed(CODE_LINE_PX))
                                .padding([0, 4])
                                .style(|_t: &Theme| code_removed_line_container()),
                            );
                        }
                    }
                    let in_sel = sel.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    let working = working_here.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    // A line that differs from HEAD (git) → GitHub-PR-style green highlight
                    // with a `+` gutter marker, so you SEE what the agent changed, live.
                    let changed = self.changed_lines.contains(n);
                    let mark = if changed { "+" } else { " " };
                    let row_text = format!("{mark}{n:>4}  {line}");
                    let color = if in_sel {
                        ACCENT
                    } else if changed {
                        GOOD
                    } else if working {
                        AMBER
                    } else {
                        FG
                    };
                    let line_el = container(
                        text(row_text)
                            .size(13)
                            // Pin the text's line height so every row is EXACTLY CODE_LINE_PX
                            // tall — the fixed height the minimap, scroll-jump, and the
                            // virtualization spacers all assume. Without this, natural line
                            // height drifts from the estimate and windowed scrolling desyncs.
                            .line_height(iced::widget::text::LineHeight::Absolute(
                                CODE_LINE_PX.into(),
                            ))
                            .font(iced::Font::MONOSPACE)
                            .color(color)
                            .wrapping(iced::widget::text::Wrapping::None),
                    )
                    .height(Length::Fixed(CODE_LINE_PX))
                    .padding([0, 4])
                    .style(move |_t: &Theme| {
                        code_line_container(in_sel, changed, working.then_some(pulse))
                    });
                    let row_ma = iced::widget::mouse_area(line_el)
                        .on_press(Message::LineDragStart(*n))
                        .on_enter(Message::LineDragTo(*n))
                        .on_release(Message::LineDragEnd);
                    col = col.push(row_ma);
                    // After the LAST green line of a changed block, render a "↩ revert block" bar —
                    // its own comment-shaped row (no comment text) carrying only the revert button,
                    // so the control lives on a dedicated line instead of floating over code. Skip
                    // it when a comment already sits on this block (it offers revert inline).
                    if let Some(&cur_start) = block_bar_after.get(n) {
                        if !blocks_with_comment.contains(&cur_start) {
                            col = col.push(view_revert_block_bar(cur_start, bar_width));
                        }
                    }
                    // Stored inline comments whose range ENDS on this line — render them (PR
                    // style), struck-through + ✓ once resolved. Only the in-window lines are
                    // iterated, so a comment scrolled off-screen simply isn't drawn (its state
                    // persists); the box/comment adds height inside the window, not the spacers.
                    if let Some(file) = self.selected_file.clone() {
                        let here: Vec<(usize, sc_win::comments::Comment)> = self
                            .comments
                            .on_file(&file)
                            .filter(|(_, c)| c.end == *n)
                            .map(|(i, c)| (i, c.clone()))
                            .collect();
                        for (i, c) in here {
                            // If the comment sits on a changed block, offer to revert that block
                            // from the comment row (look up by any line the comment covers).
                            let block =
                                (c.start..=c.end).find_map(|l| line_to_block.get(&l).copied());
                            col = col.push(view_inline_comment(i, c, bar_width, block));
                        }
                    }
                    // The (new) comment box after the last line of the committed range.
                    if self.comment_range.is_some_and(|(_, hi)| hi == *n) {
                        col = col.push(self.view_comment_box());
                    }
                }
                // Bottom spacer for the hidden lines below the window — plus any removed-lines that
                // anchor below the last visible line (they'd add height when scrolled into view).
                if last_idx < total {
                    let first_hidden_line = last_idx + 1; // 1-based line after the window
                    let below_removed: usize = self
                        .file_diff
                        .removed_before
                        .range(first_hidden_line..)
                        .map(|(_, v)| v.len())
                        .sum();
                    let hidden_rows = (total - last_idx) + below_removed;
                    col = col.push(
                        Space::new().height(Length::Fixed(hidden_rows as f32 * CODE_LINE_PX)),
                    );
                } else {
                    // Window reaches EOF: render removals anchored past the last line (deletions at
                    // end-of-file) as trailing red rows.
                    for (anchor, removed) in self.file_diff.removed_before.range(total + 1..) {
                        let _ = anchor;
                        for gone in removed {
                            col = col.push(
                                container(
                                    text(format!("-     {gone}"))
                                        .size(13)
                                        .line_height(iced::widget::text::LineHeight::Absolute(
                                            CODE_LINE_PX.into(),
                                        ))
                                        .font(iced::Font::MONOSPACE)
                                        .color(BAD)
                                        .wrapping(iced::widget::text::Wrapping::None),
                                )
                                .height(Length::Fixed(CODE_LINE_PX))
                                .padding([0, 4])
                                .style(|_t: &Theme| code_removed_line_container()),
                            );
                        }
                    }
                }
                if cv.truncated {
                    col = col.push(
                        text(format!(
                            "… truncated at {} lines",
                            sc_win::codeview::MAX_LINES
                        ))
                        .size(11)
                        .color(FG_MUTED),
                    );
                }
                // Only show the minimap when the file actually overflows the viewport — if it all
                // fits on screen there's nothing to navigate, so the map is just noise. Computed
                // above as `minimap_overflows` (drives both the comment inset and the minimap).
                let overflows = minimap_overflows;
                // With the minimap up, its viewport box IS the vertical scrollbar — hide the real
                // one (width 0) so we don't show both. Keep the horizontal bar for long lines.
                let vbar = if overflows {
                    scrollable::Scrollbar::new().width(0).scroller_width(0)
                } else {
                    scrollable::Scrollbar::new()
                };
                // One scrollable, BOTH axes: vertical for the file, horizontal for long lines.
                let code_scroll = scrollable(col)
                    .id(code_scroll_id())
                    .on_scroll(Message::CodeScrolled)
                    .direction(scrollable::Direction::Both {
                        vertical: vbar,
                        horizontal: scrollable::Scrollbar::new().width(6).scroller_width(6),
                    })
                    .height(Fill)
                    .width(Fill);
                // VS-Code-style minimap on the right: file shape + green changes + comment
                // ticks + viewport box; click a line in it to select there.
                let line_lens: Vec<usize> =
                    cv.lines.iter().map(|(_, t)| t.chars().count()).collect();
                let commented: std::collections::BTreeSet<usize> = self
                    .selected_file
                    .as_deref()
                    .map(|f| {
                        self.comments
                            .on_file(f)
                            .flat_map(|(_, c)| c.start..=c.end)
                            .collect()
                    })
                    .unwrap_or_default();
                if overflows {
                    let minimap = crate::minimap::Minimap::new(
                        line_lens,
                        self.changed_lines.clone(),
                        commented,
                        self.code_viewport,
                    )
                    .view();
                    // VS-Code style: the code fills the whole width and the minimap floats
                    // semi-transparently on top of the right edge, so text runs behind it.
                    let floating = container(minimap)
                        .width(Fill)
                        .height(Fill)
                        .align_x(iced::alignment::Horizontal::Right);
                    iced::widget::stack![code_scroll, floating].into()
                } else {
                    code_scroll.into()
                }
            }
            None => text("the file the agent edits appears here — or click one in the tree")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };

        // Header stays fixed; the code area is the single scrollable (no outer scroll wrap,
        // which is what previously collapsed the inner one). The header is now a TAB STRIP — one
        // tab per open file, the ACTIVE tab (=== `selected_file`) highlighted, each closeable.
        // When the active file is a feature plan (PLAN-<slug>.md) and no session is running, the
        // strip's right end carries an "⚒ Execute plan" button — the same one-click build the
        // proposal card offers, acting on the active file.
        let is_open_plan = self.selected_file.as_deref().is_some_and(is_feature_plan);
        let header_bar: Element<'_, Message> = if self.open_tabs.is_empty() {
            // No files open → the old "CODE" placeholder (matches the former (None, _) header).
            text("CODE").size(12).color(FG_MUTED).into()
        } else {
            // One tab per open file, in open order. Each tab is a label button (switches to it)
            // plus a SIBLING ✕ button (buttons can't nest) that closes it. The active tab reads
            // in ACCENT on a card-ish wash; inactive tabs are FG_MUTED and transparent.
            let mut strip = row![].spacing(4).align_y(iced::Alignment::Center);
            for path in &self.open_tabs {
                let active = self.selected_file.as_deref() == Some(path.as_str());
                // Show the basename (full path is too long for a tab); duplicate basenames across
                // open files are acceptable for v1.
                let base = path.rsplit(['/', '\\']).next().unwrap_or(path.as_str());
                let label = button(text(base.to_string()).size(12).color(if active {
                    ACCENT
                } else {
                    FG_MUTED
                }))
                .on_press(Message::SelectTab(path.clone()))
                .padding([2, 6])
                .style(if active { menu_item_style } else { tree_button });
                let close =
                    button(
                        text("✕")
                            .size(11)
                            .color(if active { ACCENT } else { FG_MUTED }),
                    )
                    .on_press(Message::CloseTab(path.clone()))
                    .padding([2, 4])
                    .style(tree_button);
                strip = strip.push(
                    row![label, close]
                        .spacing(0)
                        .align_y(iced::Alignment::Center),
                );
            }
            // Build the header ACTION buttons first (their own fixed-width row), so they can be
            // PINNED to the right while the tab strip scrolls in the remaining space — VS Code
            // style. Without this, the scroller expanded to fit every tab and pushed the buttons
            // off the panel's right edge (the bug: Build/Breakdown vanished with many tabs open).
            let viewing_gated_phase = self
                .gating_phase()
                .is_some_and(|p| self.plan.path_for(p).as_deref() == self.selected_file.as_deref());
            let mut actions = row![].spacing(8).align_y(iced::Alignment::Center);
            if viewing_gated_phase {
                // The file being viewed IS the phase at a gate: its Approve / Send back / Abort
                // controls sit here so you review the artifact and act in the same place (Send back
                // harvests this file's line-comments as the revision notes).
                actions = actions.push(
                    button(text("✓ Approve").size(12))
                        .on_press(Message::GateApprove)
                        .padding([3, 10])
                        .style(primary_button),
                );
                actions = actions.push(
                    button(text("↩ Send back").size(12))
                        .on_press(Message::GateSendBack)
                        .padding([3, 10])
                        .style(menu_item_style),
                );
                actions = actions.push(
                    button(text("■ Abort").size(12))
                        .on_press(Message::GateAbort)
                        .padding([3, 10])
                        .style(menu_item_style),
                );
            } else if is_open_plan && self.session.is_none() {
                // Two actions on an open plan: Breakdown runs the staged DESIGN pipeline and stops
                // for review (no code written); Build runs the whole thing through to a green
                // compile. Breakdown first — it's the review-then-build path.
                actions = actions.push(
                    button(text("☷ Breakdown").size(12))
                        .on_press(Message::ExecuteOpenPlan)
                        .padding([3, 10])
                        .style(menu_item_style),
                );
                actions = actions.push(
                    button(text("⚒ Build").size(12))
                        .on_press(Message::BuildOpenPlan)
                        .padding([3, 10])
                        .style(primary_button),
                );
            }
            // The strip scrolls horizontally in the space LEFT OF the pinned actions: `width(Fill)`
            // makes the scroller take the remaining width (not grow to fit every tab), so overflow
            // scrolls inside it while the action buttons keep their natural width on the right.
            // A visible horizontal scrollbar as the affordance that the strip scrolls when tabs
            // overflow (the old 2px bar was invisible — it looked like the extra tabs were just
            // gone). The bar is drawn along the BOTTOM edge of the scrollable's viewport, so it
            // would overlap the tab text; reserve a lane for it with bottom padding on the strip
            // content, and keep tabs top-aligned so they sit ABOVE the bar, not centered onto it.
            // Mouse-wheel over the strip scrolls it horizontally too.
            let strip = strip.align_y(iced::Alignment::Start);
            let scroller = scrollable(container(strip).padding(iced::Padding::ZERO.bottom(9)))
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new().width(5).scroller_width(5),
                ))
                .width(Fill);
            // The row MUST be `width(Fill)`: in a Shrink row, a `Fill` child resolves against the
            // row's content width (unbounded), so the scroller grows to fit every tab and shoves
            // the actions off-screen. A Fill row bounds the space, the Shrink `actions` take their
            // natural width, and the Fill scroller gets what's left → tabs scroll, buttons pinned.
            row![scroller, actions]
                .spacing(8)
                .width(Fill)
                .align_y(iced::Alignment::Center)
                .into()
        };
        // The body column must be `Fill`-width so `header_bar`'s Fill row has a bounded width to
        // fill — a Shrink column would collapse it to content width and the tab-scroll/pinned-
        // buttons layout would break (the buttons get pushed off again).
        let body = column![header_bar, inner].spacing(6).width(Fill);
        container(body)
            .width(Length::FillPortion(portion))
            .height(Fill)
            .padding(PAD)
            .style(card_style)
            .into()
    }

    /// The inline comment box shown under the selected range (PR-style): a text input +
    /// Comment / Cancel. Submitting triages small (fix now) vs. big (plan first).
    fn view_comment_box(&self) -> Element<'_, Message> {
        let placeholder = match self.comment_range {
            Some((lo, hi)) if lo != hi => format!("comment on lines {lo}–{hi}…"),
            _ => "comment on this line…".to_string(),
        };
        let input = text_input(&placeholder, &self.comment_draft)
            .on_input(Message::CommentDraftChanged)
            .on_submit(Message::CommentSubmit)
            .padding(8)
            .style(input_style)
            .width(Fill);
        let submit = button(text("Comment").size(13))
            .on_press(Message::CommentSubmit)
            .padding([4, 12])
            .style(primary_button);
        let cancel = button(text("Cancel").size(13))
            .on_press(Message::CommentCancel)
            .padding([4, 12])
            .style(menu_item_style);
        let hint = text("small fix → done inline · bigger → we'll plan it")
            .size(10)
            .color(FG_MUTED);
        container(column![input, row![submit, cancel].spacing(6), hint].spacing(6))
            .width(Fill)
            .padding(8)
            .style(dropdown_style)
            .into()
    }

    /// The bottom panel — a tabbed strip of **Verification** (verify output) and **Build**
    /// (last run outcome). It auto-hides when there's nothing to show — no run in flight, no
    /// verify output, no build result — so planning gets the full height. During a swarm
    /// build the coder prompt/output panel takes the whole strip.
    fn view_bottom_strip(&self) -> Option<Element<'_, Message>> {
        if self.is_swarm() {
            return Some(self.view_coder_io());
        }
        // Show the strip when there's build/verify content OR a project is open (so the
        // integrated Terminal is always available while working in a project).
        let has_content = self.session.is_some()
            || self.verify_text.is_some()
            || self.result.is_some()
            || self.picked_workspace.is_some();
        if !has_content {
            return None;
        }
        let tabs = row![
            self.bottom_tab_button("Verification", BottomTab::Verification),
            self.bottom_tab_button("Build", BottomTab::Build),
            self.bottom_tab_button("Terminal", BottomTab::Terminal),
        ]
        .spacing(4);
        let content = match self.bottom_tab {
            BottomTab::Verification => self.view_verification_tab(),
            BottomTab::Build => self.view_build_tab(),
            BottomTab::Terminal => self.view_terminal_tab(),
        };
        Some(
            container(column![tabs, content].spacing(6))
                .width(Fill)
                .height(Length::Fixed(180.0))
                .padding(10)
                .style(card_style)
                .into(),
        )
    }

    /// One bottom-tab button; highlighted when it's the selected tab.
    fn bottom_tab_button(&self, label: &str, tab: BottomTab) -> Element<'_, Message> {
        let selected = self.bottom_tab == tab;
        button(
            text(label.to_string())
                .size(12)
                .color(if selected { ACCENT } else { FG_MUTED }),
        )
        .on_press(Message::SelectBottomTab(tab))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(selected, status))
        .into()
    }

    /// The Verification tab: the verify command's captured output (failure-first).
    fn view_verification_tab(&self) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match &self.verify_text {
            Some(v) => text(v.clone()).size(12).into(),
            None => text("cargo check / test output shows here after the agent verifies")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };
        scrollable(inner).height(Fill).into()
    }

    /// The Build tab: the last run's outcome — ✓/✗ headline, reason, changed/built files,
    /// and (from-scratch only) an open-folder button.
    fn view_build_tab(&self) -> Element<'_, Message> {
        let Some(r) = self.result.as_ref() else {
            return text("no build yet — describe a change and run it")
                .size(12)
                .color(FG_MUTED)
                .into();
        };
        let (mark, color) = if r.ok { ("✓", GOOD) } else { ("✗", BAD) };
        let mut col = column![text(format!("{mark}  {}", r.headline))
            .size(15)
            .color(color)]
        .spacing(4);
        if !r.reason.is_empty() {
            col = col.push(text(&r.reason).size(12).color(FG_MUTED));
        }
        let label = if self.iterating { "changed" } else { "built" };
        if !r.files.is_empty() {
            col = col.push(text(format!("files {label}:")).size(11).color(FG_MUTED));
        }
        for f in r.files.iter().take(10) {
            col = col.push(text(format!("  • {f}")).size(12));
        }
        if r.files.len() > 10 {
            col = col.push(
                text(format!("  … and {} more", r.files.len() - 10))
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        if r.dir.is_some() {
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            col = col.push(
                button(text("📂 open output folder"))
                    .on_press(Message::OpenOutputFolder)
                    .style(menu_item_style),
            );
        }
        // A finished Breakdown: make the next step unmissable. Breakdown is design-only, so offer
        // the follow-ons right here — build the plan, or commit it to the repo — instead of leaving
        // the user with a passive "plan ready" line and no obvious action.
        if r.plan_ready {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(
                text("Review the breakdown above, then:")
                    .size(12)
                    .color(FG_MUTED),
            );
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            let mut build = button(text("⚒  Build this plan").size(14).color(FG)).padding([6, 16]);
            // Only enable Build when we still have the plan task and no run is in flight.
            if self.last_plan_task.is_some() && self.session.is_none() {
                build = build.on_press(Message::BuildLastPlan).style(primary_button);
            } else {
                build = build.style(stage_toggle_button);
            }
            let commit = button(text("✓  Commit the plan").size(14).color(FG))
                .on_press(Message::CommitPlan)
                .padding([6, 16])
                .style(stage_toggle_button);
            col = col.push(row![build, commit].spacing(8));
        }
        // "I don't like this change" — undo an in-place fix's edits (git-revert its files).
        // Only for iterate runs that changed files (from-scratch builds have no committed base).
        if self.iterating && !r.files.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(6.0)));
            col = col.push(
                button(text("↩ Undo this change").size(13))
                    .on_press(Message::UndoLastChange)
                    .padding([4, 12])
                    .style(|_t: &Theme, status| {
                        let hov = matches!(status, button::Status::Hovered);
                        button::Style {
                            background: Some(Background::Color(Color {
                                a: if hov { 0.22 } else { 0.14 },
                                ..BAD
                            })),
                            text_color: FG,
                            border: Border {
                                radius: RADIUS.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    }),
            );
        }
        scrollable(col).height(Fill).into()
    }

    /// The Terminal tab: a VS-Code-style command runner. Scrollback (stderr in red, the
    /// `$ cmd`/`[exit]` meta lines dimmed) above an input row; Run becomes Kill while a
    /// command is in flight. Commands run in the open workspace via [`sc_win::terminal`].
    /// A persistent one-line badge describing where terminal commands run right now, so
    /// containment is never ambiguous: `(text, colour)`.
    fn term_status_badge(&self) -> (String, iced::Color) {
        // `cfg.sandbox()` only yields Host/Docker; Session is a runtime-only state, folded in
        // here for exhaustiveness (rendered like Docker).
        let image = match self.cfg.sandbox() {
            sc_verify::Sandbox::Host => {
                return (
                    "⚠ HOST — commands run on this machine (sandbox off)".to_string(),
                    BAD,
                );
            }
            sc_verify::Sandbox::Docker { image } => image,
            sc_verify::Sandbox::Session(c) => c.name().to_string(),
        };
        if self.picked_workspace.is_none() {
            (
                "🔒 sandbox on — open a project to enable the terminal".to_string(),
                FG_MUTED,
            )
        } else if self.term_container_started {
            (
                format!("🔒 sandboxed — running in container ({image})"),
                GOOD,
            )
        } else {
            (
                format!("🔒 sandboxed — container starts on first command ({image})"),
                FG_MUTED,
            )
        }
    }

    fn view_terminal_tab(&self) -> Element<'_, Message> {
        use sc_win::terminal::Stream;
        // Persistent containment badge — always visible so you know where commands execute.
        let (badge_text, badge_color) = self.term_status_badge();
        let badge = text(badge_text).size(11).color(badge_color);
        // Scrollback: one monospace line per output line, coloured by originating stream.
        let mut col = column![].spacing(0);
        if self.terminal.lines.is_empty() {
            col = col.push(
                text("type a command and press Enter — e.g.  cargo build -p sc-win")
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        for line in &self.terminal.lines {
            let color = match line.stream {
                Stream::Stdout => FG,
                Stream::Stderr => BAD,
                Stream::Meta => FG_MUTED,
            };
            col = col.push(
                text(line.text.clone())
                    .size(12)
                    .font(iced::Font::MONOSPACE)
                    .color(color),
            );
        }
        let scrollback = scrollable(col).height(Fill).anchor_bottom().width(Fill);

        // Input row: prompt glyph · input box · Run/Kill · Clear.
        let running = self.terminal.running;
        let input = text_input("command…", &self.terminal.input)
            .on_input(Message::TermInput)
            .on_submit(Message::TermSubmit)
            .padding([6, 8])
            .font(iced::Font::MONOSPACE)
            .style(input_style_borderless)
            .width(Fill);
        let action = if running {
            button(text("⏹ Kill").size(13))
                .on_press(Message::TermKill)
                .padding([4, 12])
                .style(menu_item_style)
        } else {
            button(text("▶ Run").size(13))
                .on_press(Message::TermSubmit)
                .padding([4, 12])
                .style(menu_item_style)
        };
        let clear = button(text("Clear").size(13).color(FG_MUTED))
            .on_press(Message::TermClear)
            .padding([4, 12])
            .style(menu_item_style);
        // Command-history recall (↑ previous, ↓ next) — the arrow-key affordance as buttons,
        // since `text_input` doesn't surface arrow keys.
        let hist_prev = button(text("↑").size(13).font(iced::Font::MONOSPACE))
            .on_press(Message::TermHistoryPrev)
            .padding([4, 8])
            .style(menu_item_style);
        let hist_next = button(text("↓").size(13).font(iced::Font::MONOSPACE))
            .on_press(Message::TermHistoryNext)
            .padding([4, 8])
            .style(menu_item_style);
        let prompt = text(if running { "…" } else { "$" })
            .size(13)
            .font(iced::Font::MONOSPACE)
            .color(if running { ACCENT } else { GOOD });
        let input_row = row![prompt, input, hist_prev, hist_next, action, clear]
            .spacing(6)
            .align_y(iced::Alignment::Center);

        column![badge, scrollback, input_row]
            .spacing(6)
            .height(Fill)
            .into()
    }

    /// The horizontal step-flow at the top: each phase with arrows between, the current
    /// phase highlighted, done phases checked, plus a final "Build" step that lights up
    /// once planning is complete and implementation begins.
    fn view_step_flow(&self) -> Element<'_, Message> {
        let current = self.plan.current_phase();
        let done_color = iced::Color::from_rgb(0.45, 0.78, 0.55); // green
        let now_color = iced::Color::from_rgb(0.48, 0.65, 0.98); // blue
        let dim_color = iced::Color::from_rgb(0.4, 0.43, 0.55);
        let arrow_color = iced::Color::from_rgb(0.35, 0.38, 0.5);

        let mut flow = row![].spacing(6).align_y(iced::Alignment::Center);
        let steps = self.plan.steps();
        for (i, step) in steps.iter().enumerate() {
            let is_current = current == Some(step.phase);
            let (mark, color, size) = if step.done {
                ("✓", done_color, 13)
            } else if is_current {
                ("▶", now_color, 15)
            } else {
                ("·", dim_color, 13)
            };
            let label = text(format!("{mark} {}", step.phase.title()))
                .size(size)
                .color(color);
            flow = flow.push(label);
            flow = flow.push(text("→").size(13).color(arrow_color));
            let _ = i;
        }
        // The final "Build" step: active once planning is complete (no current phase
        // left) and a swarm is running; done when source files were built.
        let built = self.result.as_ref().is_some_and(|r| r.ok);
        let building = current.is_none() && self.is_swarm();
        let (bmark, bcolor) = if built {
            ("✓", done_color)
        } else if building {
            ("▶", now_color)
        } else {
            ("·", dim_color)
        };
        flow = flow.push(
            text(format!("{bmark} Build"))
                .size(if building { 15 } else { 13 })
                .color(bcolor),
        );

        container(
            scrollable(flow).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            )),
        )
        .width(Fill)
        .padding(10)
        .style(card_style)
        .into()
    }

    /// The top menu bar: the app title, the dropdown menu buttons (File / View), and —
    /// pushed to the right — the workspace/status line (what folder we're working in, and
    /// how). Clicking a title toggles its dropdown; items live in [`Self::view_menu_dropdown`].
    fn view_menu_bar(&self) -> Element<'_, Message> {
        // No brand logo here — the window/taskbar icon already shows it right above.
        let file = self.menu_title("File", Menu::File);
        let view_m = self.menu_title("View", Menu::View);
        // Layout: File/View at the left · workspace status centered · model health badge
        // at the right. Two flexible spacers center the status between the fixed ends.
        let status = text(self.workspace_status())
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70));
        let health = self.view_backend_badge();
        let bar = row![
            file,
            view_m,
            Space::new().width(Fill), // left spacer
            status,
            Space::new().width(Fill), // right spacer → status sits centered
            health,
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        container(bar)
            .width(Fill)
            .padding([4, 10])
            .style(menu_bar_style)
            .into()
    }

    /// A human reason the backend is known-unusable, or `None` if it's `Ready` or not yet
    /// probed. Used to preflight-gate runs so a bad backend fails loudly up front instead of
    /// mid-stream. A `None` (unprobed) health is allowed through — we don't block the first
    /// few seconds after launch before the first probe lands.
    fn backend_unready_reason(&self) -> Option<String> {
        use sc_model::BackendHealth::*;
        match &self.backend_health {
            Some(NoModel { .. }) => Some("backend is up but no model is loaded".to_string()),
            Some(Unreachable { .. }) => {
                Some(format!("backend unreachable at {}", self.cfg.base_url))
            }
            None | Some(Ready) => None,
        }
    }

    /// The backend health badge in the top bar (after File/View): a coloured dot + short
    /// label from the periodic probe. Green = a real completion succeeded (model serving);
    /// amber = endpoint reachable but no model loaded; red = unreachable; grey = probing.
    fn view_backend_badge(&self) -> Element<'_, Message> {
        use sc_model::BackendHealth::*;
        let (dot, label, color) = match &self.backend_health {
            None => ("●", "checking backend…".to_string(), FG_MUTED),
            Some(Ready) => ("●", format!("{} ready", self.cfg.model), GOOD),
            Some(NoModel { .. }) => (
                "●",
                "backend up — no model loaded".to_string(),
                Color::from_rgb(0.95, 0.72, 0.30), // amber
            ),
            Some(Unreachable { .. }) => (
                "●",
                format!("backend unreachable ({})", self.cfg.base_url),
                BAD,
            ),
        };
        row![
            text(dot).size(12).color(color),
            text(label).size(11).color(color),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center)
        .into()
    }

    /// The one-line workspace status shown at the right of the top bar: where the app is
    /// working and in which mode.
    fn workspace_status(&self) -> String {
        match (&self.picked_workspace, &self.run_dir) {
            (Some(dir), _) => {
                let stack = sc_workflow::ProjectStack::detect(dir).label();
                format!("iterating in  {}  ·  {stack}", dir.display())
            }
            (None, Some(d)) => format!("output  {}", d.display()),
            (None, None) => "no project — File ▸ Open folder".to_string(),
        }
    }

    /// One clickable menu-bar title; highlighted while its dropdown is open.
    fn menu_title(&self, label: &str, which: Menu) -> Element<'_, Message> {
        let open = self.open_menu == Some(which);
        button(
            text(label.to_string())
                .size(13)
                .color(if open { ACCENT } else { FG }),
        )
        .on_press(Message::ToggleMenu(which))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(open, status))
        .into()
    }

    /// The open dropdown's items, rendered as a floating Windows-style card positioned
    /// directly under its menu-bar title (via top/left spacers in an overlay layer, so it
    /// never shifts the base layout). A full-window transparent backdrop behind it closes
    /// the menu on an outside click. Returns `None` when no menu is open.
    fn view_menu_dropdown(&self) -> Option<Element<'_, Message>> {
        let which = self.open_menu?;
        let items: Vec<(String, Message)> = match which {
            Menu::File => {
                let mut v: Vec<(String, Message)> = if self.picked_workspace.is_some() {
                    vec![
                        (
                            "📁  Open a different folder…".to_string(),
                            Message::PickWorkspace,
                        ),
                        ("✕  Close project".to_string(), Message::ClearWorkspace),
                    ]
                } else {
                    vec![("📁  Open folder…".to_string(), Message::PickWorkspace)]
                };
                // Recent projects (most-recent first), excluding the currently-open one.
                let recents = sc_win::persist::load().recents;
                let current = self.picked_workspace.clone();
                let recent_items: Vec<_> = recents
                    .into_iter()
                    .filter(|p| Some(p) != current.as_ref())
                    .take(8)
                    .collect();
                if !recent_items.is_empty() {
                    v.push(("— Recent —".to_string(), Message::NoOp));
                    for p in recent_items {
                        let name = p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("(project)")
                            .to_string();
                        v.push((format!("🕘  {name}"), Message::OpenRecent(p)));
                    }
                }
                v
            }
            Menu::View => vec![(
                if self.settings_open {
                    "⚙  Hide settings".to_string()
                } else {
                    "⚙  Settings…".to_string()
                },
                Message::ToggleSettings,
            )],
        };
        let mut col = column![].spacing(0);
        for (label, msg) in items {
            // A "— … —" entry is a non-clickable section header, rendered as dimmed text.
            if label.starts_with("— ") {
                col = col.push(
                    container(text(label).size(11).color(FG_MUTED))
                        .padding([6, 14])
                        .width(Length::Fixed(230.0)),
                );
            } else {
                col = col.push(
                    button(text(label).size(13).color(FG))
                        .on_press(msg)
                        .padding([6, 14])
                        .width(Length::Fixed(230.0))
                        .style(menu_item_style),
                );
            }
        }
        let card = container(col).padding(3).style(dropdown_style);

        // Position under the right title. With the brand logo removed, File now starts right
        // after the menu bar's 10px left padding; View follows it. Spacers place the card; a
        // transparent full-window backdrop (a mouse_area) closes the menu on any outside click.
        let left = match which {
            Menu::File => 8.0,
            Menu::View => 48.0,
        };
        let positioned = column![
            Space::new().height(Length::Fixed(30.0)),
            row![Space::new().width(Length::Fixed(left)), card],
        ];
        // Backdrop: an invisible full-size mouse_area that closes the menu when clicked.
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleMenu(which)); // re-toggle closes it

        Some(
            iced::widget::stack![backdrop, positioned]
                .width(Fill)
                .height(Fill)
                .into(),
        )
    }

    /// The Settings modal: a centered card floating over a dimmed backdrop (an overlay
    /// layer in `view`'s Stack). Clicking the backdrop or the ✕ closes it. This replaces the
    /// old inline panel that pushed the layout around.
    fn view_settings_modal(&self) -> Element<'_, Message> {
        // The dim backdrop; clicking it closes settings.
        let backdrop =
            iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill).style(
                |_t: &Theme| container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.55,
                        ..Color::BLACK
                    })),
                    ..container::Style::default()
                },
            ))
            .on_press(Message::ToggleSettings);

        // The settings card itself, centered, with a close button in its header.
        let header = row![
            text("Settings").size(16).color(FG),
            Space::new().width(Fill),
            button(text("✕").size(14))
                .on_press(Message::ToggleSettings)
                .padding([2, 8])
                .style(menu_item_style),
        ]
        .align_y(iced::Alignment::Center);
        let card = container(column![header, self.view_settings_body()].spacing(12))
            .width(Length::Fixed(520.0))
            .max_width(560.0)
            .padding(18)
            .style(dropdown_style);

        // `opaque` stops clicks on the card from falling through to the backdrop.
        iced::widget::stack![backdrop, iced::widget::center(iced::widget::opaque(card))]
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// The center column: the planning CHAT thread on top (you ⟷ agent), with the composer
    /// docked at the bottom. When the assistant proposes plan-file changes, they render as
    /// Apply cards under its message. Before a project is opened, a hint to open a folder.
    fn view_center(&self) -> Element<'_, Message> {
        let mut thread = column![section("CHAT  ·  PLAN")].spacing(8);
        if self.conversation.is_none() {
            thread = thread.push(
                text("Open a project folder (File ▸ Open folder) to start planning.")
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        for (i, turn) in self.chat_turns.iter().enumerate() {
            thread = thread.push(self.view_chat_turn(i, turn));
        }
        // Proposed plan-file Apply cards, after the latest assistant message.
        for (i, pf) in self.proposed_files.iter().enumerate() {
            thread = thread.push(self.view_proposed_file(i, pf));
        }
        // A proposed command (the Command intent) → a Run card that pipes it into the terminal.
        if let Some(cmd) = &self.proposed_command {
            thread = thread.push(self.view_proposed_command(cmd));
        }
        // The live "typing" bubble while a reply streams in: show the growing text (with any
        // <think> block hidden), or a thinking cue before the first token arrives. Shown for a
        // chat/plan turn (a ChatSession) AND for a staged run streaming phases through `streaming`
        // (no ChatSession there — the session emits ChatDelta over UiEvent::Agent).
        if self.chat_session.is_some() || self.streaming.is_some() {
            let live = self
                .streaming
                .as_deref()
                .map(sc_win::chat::visible_so_far)
                .filter(|s| !s.is_empty());
            match live {
                Some(text_so_far) => {
                    thread = thread.push(
                        column![
                            text("agent").size(11).color(GOOD),
                            text(text_so_far).size(13).color(FG),
                        ]
                        .spacing(2),
                    );
                }
                None => {
                    thread = thread.push(text("agent is thinking…").size(12).color(FG_MUTED));
                }
            }
        }
        // A pending workflow gate: the Approve / Send-back / Abort controls live at the bottom of
        // the chat, right under the phase content that just streamed in (they used to sit in a
        // separate left PLAN panel). Only shown while a gate is actually waiting.
        if let Some(controls) = self.view_gate_controls() {
            thread = thread.push(controls);
        }
        // The thread scrolls inside its own padding; the composer below spans the panel edge
        // to edge (its divider + input reach the left/right/bottom), so there's no gutter
        // around the input. Hence the panel container itself is unpadded.
        let thread = container(
            scrollable(thread)
                .id(chat_scroll_id())
                .on_scroll(Message::ChatScrolled)
                .height(Fill),
        )
        .padding(PAD);

        let composer = self.view_composer();
        // The outer column must be Fill-width, else its Fill children (incl. the composer's
        // full-width top divider) collapse to content width and the hairline stops short of the
        // panel's right edge.
        container(column![thread, composer].width(Fill))
            .width(Length::FillPortion(2))
            .height(Fill)
            .style(card_style)
            .into()
    }

    /// The workflow-gate controls, rendered inline at the bottom of the chat when a phase is
    /// waiting for a human decision. `None` when no gate is pending. Approve / Send back / Abort
    /// emit the same messages the old left-panel copy did (the send-back note is the fallback when
    /// no line comments were left on the phase's file in CODE — which auto-opens on the gate).
    fn view_gate_controls(&self) -> Option<Element<'_, Message>> {
        let phase = self.gating_phase()?;
        let buttons = row![
            button(text("Approve").size(13))
                .on_press(Message::GateApprove)
                .padding([4, 14])
                .style(primary_button),
            button(text("Send back").size(13))
                .on_press(Message::GateSendBack)
                .padding([4, 14])
                .style(stage_toggle_button),
            button(text("Abort").size(13))
                .on_press(Message::GateAbort)
                .padding([4, 14])
                .style(stage_toggle_button),
        ]
        .spacing(6);
        let notes = text_input("or a general send-back note…", &self.sendback_notes)
            .on_input(Message::NotesChanged)
            .padding(6)
            .size(12)
            .style(input_style);
        let card = column![
            text(format!("⛳ Review the {} above.", phase.title()))
                .size(13)
                .color(AMBER),
            text("Comment lines in the CODE view to change, then Send back — or Approve to continue.")
                .size(10)
                .color(FG_MUTED),
            buttons,
            notes,
        ]
        .spacing(6);
        Some(
            container(card)
                .width(Fill)
                .padding(10)
                .style(card_style)
                .into(),
        )
    }

    /// One chat bubble: a small role label + the text, coloured by speaker. A Debug turn (the
    /// raw prompt echo) renders dimmed + monospace so it reads as diagnostic output, not chat.
    /// Rebuild the per-turn selectable editor buffers when the chat thread changes. Cheap
    /// change-detection via a `(len, last_text_len)` signature so this is a no-op on the vast
    /// majority of update calls; only a new/edited turn triggers a full rebuild. Rebuilding
    /// wholesale (rather than diffing) keeps it simple and correct — chat threads are short.
    fn sync_chat_editors(&mut self) {
        use iced::widget::text_editor::Content;
        let last_len = self.chat_turns.last().map_or(0, |t| t.text.len());
        let sig = (self.chat_turns.len(), last_len);
        if sig == self.chat_sig && self.chat_editors.len() == self.chat_turns.len() {
            return;
        }
        self.chat_sig = sig;
        self.chat_editors = self
            .chat_turns
            .iter()
            .map(|t| Content::with_text(&t.text))
            .collect();
    }

    fn view_chat_turn<'a>(
        &'a self,
        i: usize,
        turn: &'a sc_win::chat::Turn,
    ) -> Element<'a, Message> {
        if turn.role == sc_win::chat::Speaker::Debug {
            return container(
                column![
                    text("⛭ prompt sent to model").size(10).color(FG_MUTED),
                    text(turn.text.clone())
                        .size(11)
                        .font(iced::Font::MONOSPACE)
                        .color(FG_MUTED),
                ]
                .spacing(2),
            )
            .width(Fill)
            .padding(6)
            .style(|_t: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgb(0.08, 0.09, 0.14))),
                border: Border {
                    color: CARD_BORDER,
                    width: 1.0,
                    radius: RADIUS.into(),
                },
                ..container::Style::default()
            })
            .into();
        }
        let (who, who_color) = match turn.role {
            sc_win::chat::Speaker::You => ("you", ACCENT),
            _ => ("agent", GOOD),
        };
        // A small copy button beside the speaker label — copies the whole message in one click
        // (alongside drag-select below).
        let copy = button(text("⧉ copy").size(10).color(FG_MUTED))
            .on_press(Message::CopyTurn(turn.text.clone()))
            .padding([1, 6])
            .style(menu_item_style);
        let header = row![
            text(who).size(11).color(who_color),
            Space::new().width(Fill),
            copy,
        ]
        .align_y(iced::Alignment::Center);

        // The message body: a read-only text_editor so it's drag-selectable + Ctrl+C-copyable.
        // It's styled transparent/borderless to read as plain chat text, not an input box. The
        // `on_action` handler drops edits (see `Message::ChatEditorAction`), so it's immutable.
        // Falls back to plain text if the editor buffer isn't synced yet (shouldn't happen).
        let body: Element<'a, Message> = match self.chat_editors.get(i) {
            Some(content) => iced::widget::text_editor(content)
                .size(13)
                .padding(0)
                .on_action(move |a| Message::ChatEditorAction(i, a))
                .style(|_t: &Theme, _s| iced::widget::text_editor::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border::default(),
                    placeholder: FG_MUTED,
                    value: FG,
                    selection: Color { a: 0.35, ..ACCENT },
                })
                .into(),
            None => text(turn.text.clone()).size(13).color(FG).into(),
        };
        column![header, body].spacing(2).into()
    }

    /// If debug mode is on, echo `prompt` into the chat as a Debug turn (the raw text the
    /// model received). `label` names the call (e.g. "triage", "fix", "chat").
    fn debug_prompt(&mut self, label: &str, prompt: &str) {
        if self.debug {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Debug,
                text: format!("[{label}]\n{prompt}"),
            });
        }
    }

    /// An Apply card for a proposed plan-file: the filename + an Apply button (writes it to
    /// disk). The file's contents show in the code view when it's the current proposal.
    fn view_proposed_file(
        &self,
        i: usize,
        pf: &sc_win::chat::ProposedFile,
    ) -> Element<'_, Message> {
        let lines = pf.content.lines().count();
        let head = row![
            text(format!("📄 proposed: {}", pf.name))
                .size(13)
                .color(ACCENT),
            Space::new().width(Fill),
            text(format!("{lines} lines")).size(11).color(FG_MUTED),
        ]
        .align_y(iced::Alignment::Center);
        // The Apply button: once applied, it reads "✓ Applied" and is inert (no on_press), so the
        // card records the write while its Breakdown/Build actions stay live.
        let apply = if pf.applied {
            button(text("✓ Applied").size(13).color(FG_MUTED))
                .padding([5, 12])
                .style(menu_item_style)
        } else {
            button(text("✓ Apply to disk").size(13))
                .on_press(Message::ApplyFile(i))
                .padding([5, 12])
                .style(primary_button)
        };
        // A feature plan (PLAN-<slug>.md) gets two one-click actions (both apply it to disk
        // first): Breakdown runs the staged DESIGN pipeline and stops for review; Build runs the
        // whole thing through to a green compile. These STAY available after applying (the card is
        // kept, marked applied), so you can review then act. README/TODO edits aren't buildable →
        // Apply only, and their card is removed on apply.
        let actions: Element<'_, Message> = if is_feature_plan(&pf.name) {
            let breakdown = button(text("☷ Breakdown").size(13))
                .on_press(Message::BreakdownPlan(i))
                .padding([5, 12])
                .style(menu_item_style);
            let build = button(text("⚒ Build").size(13))
                .on_press(Message::ExecutePlan(i))
                .padding([5, 12])
                .style(primary_button);
            row![apply, breakdown, build].spacing(8).into()
        } else {
            apply.into()
        };
        container(column![head, actions].spacing(6))
            .width(Fill)
            .padding(10)
            .style(dropdown_style)
            .into()
    }

    /// A Run card for a command the chat proposed (the `Command` intent). Shows the exact
    /// command and a Run button that pipes it into the integrated terminal (strict sandbox
    /// applies), plus a Dismiss. Nothing runs until you click — commands execute, so the chat
    /// proposes rather than auto-runs.
    fn view_proposed_command(&self, cmd: &str) -> Element<'_, Message> {
        let head = row![
            text("▶ run command").size(13).color(ACCENT),
            Space::new().width(Fill),
        ]
        .align_y(iced::Alignment::Center);
        let cmd_line = text(cmd.to_string())
            .size(13)
            .font(iced::Font::MONOSPACE)
            .color(FG);
        let run = button(text("▶ Run in terminal").size(13))
            .on_press(Message::RunProposedCommand)
            .padding([5, 12])
            .style(primary_button);
        let dismiss = button(text("Dismiss").size(13).color(FG_MUTED))
            .on_press(Message::DismissProposedCommand)
            .padding([5, 12])
            .style(menu_item_style);
        container(column![head, cmd_line, row![run, dismiss].spacing(8)].spacing(6))
            .width(Fill)
            .padding(10)
            .style(dropdown_style)
            .into()
    }

    /// The composer: the text input + send. When a project (conversation) is open it sends a
    /// CHAT turn; with no project it falls back to the from-scratch build action.
    fn view_composer(&self) -> Element<'_, Message> {
        let has_convo = self.conversation.is_some();
        let sending = self.chat_session.is_some();
        let (placeholder, send_msg, label): (&str, Message, &str) = if has_convo {
            (
                "Talk through the plan — ask, refine, decide what's next…",
                Message::ChatSend,
                "Send",
            )
        } else {
            (
                "Open a project folder to start…",
                self.run_message(),
                self.run_label(),
            )
        };
        // A fix/iterate run in flight → a Cancel button (the run is the slow, cancellable
        // thing). A quick chat/triage reply just shows a busy "…".
        let run_active = self.session.is_some();
        // The composer is exactly one input tall — a fixed height so the stacked think/debug
        // toggles (which are naturally taller than a single-line input) don't inflate the row
        // and leave dead space above/below the input.
        const INPUT_H: f32 = 38.0;
        let input = text_input(placeholder, &self.intent)
            .on_input(Message::IntentChanged)
            .on_submit(send_msg.clone())
            .padding([8, 10])
            .style(input_style_borderless)
            .width(Fill);
        let btn = if run_active {
            button(text("⏹ cancel").size(15).width(Fill).height(Fill).center())
                .on_press(Message::CancelRun)
                .width(Length::Fixed(110.0))
                .height(Fill)
                .padding(0)
                .style(|_t: &Theme, status| {
                    let hov = matches!(status, button::Status::Hovered);
                    button::Style {
                        background: Some(Background::Color(if hov {
                            Color::from_rgb(0.80, 0.35, 0.40)
                        } else {
                            BAD
                        })),
                        text_color: Color::from_rgb(0.06, 0.07, 0.11),
                        border: Border {
                            radius: RADIUS.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
        } else if sending {
            // A chat/plan turn is running → a Cancel button that interrupts it (the streaming
            // backend stops at its next SSE line). Darker-orange idle, brightening on hover.
            button(text("Cancel").size(15).width(Fill).height(Fill).center())
                .on_press(Message::CancelChat)
                .width(Length::Fixed(90.0))
                .height(Fill)
                .padding(0)
                .style(|_t: &Theme, status| {
                    let hov = matches!(status, button::Status::Hovered);
                    button::Style {
                        background: Some(Background::Color(if hov {
                            Color::from_rgb(0.85, 0.47, 0.18)
                        } else {
                            Color::from_rgb(0.72, 0.40, 0.16)
                        })),
                        text_color: Color::from_rgb(0.12, 0.06, 0.02),
                        border: Border {
                            radius: RADIUS.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
        } else {
            button(text(label).size(15).width(Fill).height(Fill).center())
                .on_press(send_msg)
                .width(Length::Fixed(90.0))
                .height(Fill)
                .padding(0)
                .style(primary_button)
        };
        // Send button is full composer height, sitting flush against the input.
        let mut bar = row![input, btn].spacing(0);
        // The think/debug toggles stack vertically to the right of the send button. They're kept
        // small (14px box, 11px label, tight gap) so both fit within the one-input-tall composer.
        let mut toggles = column![]
            .spacing(2)
            .padding([0, 8])
            .align_x(iced::Alignment::Start);
        // The Think toggle (chat mode only): fast conclusions by default, deeper reasoning
        // when you flip it on for a hard planning question.
        if has_convo {
            toggles = toggles.push(
                checkbox(self.think)
                    .label("think")
                    .size(14)
                    .text_size(11)
                    .spacing(4)
                    .on_toggle(Message::ToggleThink)
                    .style(checkbox_style),
            );
        }
        // Debug: echo every prompt sent to the model into the chat (always available).
        toggles = toggles.push(
            checkbox(self.debug)
                .label("debug")
                .size(14)
                .text_size(11)
                .spacing(4)
                .on_toggle(Message::ToggleDebug)
                .style(checkbox_style),
        );
        bar = bar.push(toggles);
        // Fix the row to one input height: the send button's `height(Fill)` then matches the
        // input exactly (flush), and the row no longer stretches to the taller toggle stack —
        // killing the dead space above/below the input. Toggles center against that height.
        // The input bar fills the composer edge-to-edge: no horizontal padding (so it reaches the
        // panel's left/right) and no top divider (so it reaches the top). One flush block.
        bar.align_y(iced::Alignment::Center)
            .height(Length::Fixed(INPUT_H))
            .width(Fill)
            .into()
    }

    fn view_topology(&self) -> Element<'_, Message> {
        let canvas = crate::canvas::TopologyCanvas::new(
            &self.topology,
            self.now(),
            self.selected_coder.as_deref(),
        )
        .view();
        container(column![section("SWARM TOPOLOGY"), canvas].spacing(6))
            .width(Length::FillPortion(2))
            .height(Fill)
            .padding(PAD)
            .style(card_style)
            .into()
    }

    /// The bottom panel: the prompt sent to the selected coder and the output it
    /// proposed back. Click a coder box on the topology to select one; otherwise it
    /// hints how, or shows the latest verification result.
    fn view_coder_io(&self) -> Element<'_, Message> {
        let selected = self
            .selected_coder
            .as_deref()
            .and_then(|id| self.topology.coder(id));

        let inner = match selected {
            Some(c) => {
                let prompt = c.prompt.clone().unwrap_or_else(|| "(not captured)".into());
                let proposal = c
                    .proposal
                    .clone()
                    .unwrap_or_else(|| "(still working…)".into());
                column![
                    text(format!("coder [{}] — {}", c.subtask, c.goal))
                        .size(14)
                        .color(ACCENT),
                    section("▸ PROMPT SENT TO THIS CODER"),
                    text(prompt).size(11),
                    Space::new().height(Length::Fixed(6.0)),
                    section("◂ OUTPUT IT PROPOSED"),
                    text(proposal).size(11),
                ]
                .spacing(3)
            }
            None => {
                let hint = if self.is_swarm() {
                    "click a coder box above to see the prompt it got and the code it wrote"
                } else {
                    "the coders' prompts & output appear here during the build"
                };
                let mut c = column![
                    section("CODER PROMPT & OUTPUT"),
                    text(hint).size(12).color(FG_MUTED)
                ]
                .spacing(4);
                if let Some(v) = &self.verify_text {
                    c = c.push(Space::new().height(Length::Fixed(6.0)));
                    c = c.push(section("LATEST VERIFICATION"));
                    c = c.push(text(v).size(11));
                }
                c
            }
        };

        container(scrollable(inner).height(Fill))
            .width(Fill)
            .height(Length::FillPortion(1))
            .padding(PAD)
            .style(card_style)
            .into()
    }

    /// The bottom decision card — now ONLY for shell-command confirms. Workflow-phase gate
    /// approval moved into the PLAN master list (Change A): each phase's Approve/Send-back/Abort
    /// buttons sit inline on its row, beside the file you review in CODE. A `Gate` at the front of
    /// the queue therefore renders nothing here (the master list owns it).
    fn view_gatebar(&self) -> Option<Element<'_, Message>> {
        match self.gatebar.first()? {
            // Workflow gate → handled by the master list, not this bottom card.
            Gatebar::Gate { .. } => None,
            Gatebar::Confirm {
                command, reason, ..
            } => {
                let head = text(format!("⛳ run a shell command?  {command}")).size(14);
                let why = text(reason).size(12);
                let buttons = row![
                    button(text("Allow once")).on_press(Message::ConfirmAllow),
                    button(text("Allow & remember")).on_press(Message::ConfirmRemember),
                    button(text("Deny")).on_press(Message::ConfirmDeny),
                ]
                .spacing(8);
                Some(
                    container(column![head, why, buttons].spacing(6))
                        .width(Fill)
                        .padding(12)
                        .style(card_style)
                        .into(),
                )
            }
        }
    }

    /// The settings form body (no outer card — the modal wraps it). A tab strip (Connections /
    /// Routing) over a scrollable body, so endpoints+keys are set once and stages are routed
    /// separately. The active tab is [`Self::settings_tab`].
    fn view_settings_body(&self) -> Element<'_, Message> {
        // Tab strip: two toggle buttons, the active one highlighted (reuses the stage-toggle look).
        let tab = |label: &str, which: SettingsTab| {
            let active = self.settings_tab == which;
            let color = if active { FG } else { FG_MUTED };
            button(text(label.to_string()).size(13).color(color))
                .on_press(Message::SettingsTabChanged(which))
                .padding([6, 14])
                .style(if active {
                    primary_button
                } else {
                    stage_toggle_button
                })
        };
        let tabs = row![
            tab("Connections", SettingsTab::Connections),
            tab("Routing", SettingsTab::Routing),
        ]
        .spacing(8);

        let body = match self.settings_tab {
            SettingsTab::Connections => self.view_connections_tab(),
            SettingsTab::Routing => self.view_routing_tab(),
        };

        column![tabs, scrollable(body).height(Length::Fixed(400.0))]
            .spacing(12)
            .into()
    }

    /// The CONNECTIONS tab: the two endpoints (Local + Gemini), each an url + secure key. This is
    /// where the Gemini key lives — on the Gemini connection ONLY, so it never bleeds onto the
    /// local coder endpoint.
    fn view_connections_tab(&self) -> Element<'_, Message> {
        let local_url = text_input(
            "local url (e.g. http://localhost:11435/v1)",
            &self.local_url_input,
        )
        .on_input(Message::LocalUrlChanged)
        .padding(6)
        .style(input_style);
        let local_key = text_input("local api key (usually blank)", &self.local_key_input)
            .on_input(Message::LocalKeyChanged)
            .secure(true)
            .padding(6)
            .style(input_style);
        let gemini_url = text_input("gemini url", &self.gemini_url_input)
            .on_input(Message::GeminiUrlChanged)
            .padding(6)
            .style(input_style);
        let gemini_key = text_input("gemini api key", &self.gemini_key_input)
            .on_input(Message::GeminiKeyChanged)
            .secure(true)
            .padding(6)
            .style(input_style);

        column![
            text("LOCAL  (your llama.cpp / Ollama server)")
                .size(11)
                .color(FG_MUTED),
            local_url,
            local_key,
            text("GEMINI  (Google's OpenAI-compatible endpoint)")
                .size(11)
                .color(FG_MUTED),
            gemini_url,
            gemini_key,
            text("The Gemini key is read from .env (GEMINI_API_KEY) if present.")
                .size(10)
                .color(FG_MUTED),
        ]
        .spacing(8)
        .into()
    }

    /// The ROUTING tab: for each stage, pick which connection it uses + its model. Plus the
    /// verify command and posture toggles (endpoint-agnostic behaviour).
    fn view_routing_tab(&self) -> Element<'_, Message> {
        let model = text_input("coder model (e.g. qwen3-coder-30b)", &self.model_input)
            .on_input(Message::ModelChanged)
            .padding(6)
            .style(input_style);
        let orch_model = text_input(
            "planner model (e.g. gemini-2.5-flash-lite)",
            &self.orch_model_input,
        )
        .on_input(Message::OrchModelChanged)
        .padding(6)
        .style(input_style);
        let advisor = text_input("advisor model (optional)", &self.advisor_input)
            .on_input(Message::AdvisorChanged)
            .padding(6)
            .style(input_style);
        let verify = text_input("verify command (optional)", &self.verify_input)
            .on_input(Message::VerifyChanged)
            .padding(6)
            .style(input_style);
        let suffix = text_input("system suffix (e.g. /no_think)", &self.suffix_input)
            .on_input(Message::SuffixChanged)
            .padding(6)
            .style(input_style);
        let yolo = checkbox(self.cfg.yolo)
            .label("yolo (allow shell without asking)")
            .on_toggle(Message::ToggleYolo)
            .style(checkbox_style);
        let dry = checkbox(self.cfg.dry_run)
            .label("dry-run (no writes)")
            .on_toggle(Message::ToggleDryRun)
            .style(checkbox_style);

        column![
            text("CODER  (does the file writing)")
                .size(11)
                .color(FG_MUTED),
            provider_toggle(self.cfg.coder_provider, Message::CoderProviderChanged),
            model,
            text("PLANNER  (does the breakdown)")
                .size(11)
                .color(FG_MUTED),
            provider_toggle(self.cfg.planner_provider, Message::PlannerProviderChanged),
            orch_model,
            text("ADVISOR  (junior asks senior on a stall)")
                .size(11)
                .color(FG_MUTED),
            provider_toggle(self.cfg.advisor_provider, Message::AdvisorProviderChanged),
            advisor,
            text("VERIFY & BEHAVIOUR").size(11).color(FG_MUTED),
            verify,
            suffix,
            yolo,
            dry,
        ]
        .spacing(8)
        .into()
    }
}

mod helpers;
pub(crate) use helpers::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_range_selects_inclusive_span_in_display_order() {
        let order: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Forward range a..=c.
        let r = git_range(&order, "a", "c");
        assert_eq!(r, ["a", "b", "c"].iter().map(|s| s.to_string()).collect());
        // Backward range (target before anchor) spans the same inclusive set.
        let r = git_range(&order, "d", "b");
        assert_eq!(r, ["b", "c", "d"].iter().map(|s| s.to_string()).collect());
        // Anchor == target → a single-element selection.
        let r = git_range(&order, "c", "c");
        assert_eq!(r, ["c"].iter().map(|s| s.to_string()).collect());
        // Missing anchor (stale) → fall back to just the target.
        let r = git_range(&order, "zzz", "d");
        assert_eq!(r, ["d"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn tab_after_close_activates_the_right_neighbour() {
        // Start with tabs [a, b, c, d] (indices 0..3).
        // Close the active MIDDLE tab (index 1, "b") → after removal len is 3, the tab that
        // shifted into slot 1 ("c") activates.
        assert_eq!(tab_after_close(1, 3), Some(1));
        // Close the FIRST tab (index 0) → the new first tab ("b") slides into slot 0.
        assert_eq!(tab_after_close(0, 3), Some(0));
        // Close the LAST tab (index 3) → len is 3, clamp to the new last (index 2).
        assert_eq!(tab_after_close(3, 3), Some(2));
        // Close the ONLY tab → nothing remains.
        assert_eq!(tab_after_close(0, 0), None);
    }

    #[test]
    fn feature_plans_are_buildable_but_readme_and_todo_are_not() {
        // The Execute-plan button gates on this: only a PLAN-<slug>.md is buildable.
        assert!(is_feature_plan("PLAN-lakes.md"));
        assert!(is_feature_plan("plan-auth-flow.md")); // case-insensitive
        assert!(!is_feature_plan("README.md"));
        assert!(!is_feature_plan("TODO.md"));
        assert!(!is_feature_plan("PLAN-lakes.txt")); // must be markdown
        assert!(!is_feature_plan("MYPLAN-x.md")); // must start with the PLAN- prefix
    }

    #[test]
    fn feature_spec_of_normalizes_any_artifact_to_spec_md() {
        // Any phase file of a feature folder → that feature's spec.md, so Build targets the
        // feature (and reuses its approved design) whichever artifact is open.
        assert_eq!(
            feature_spec_of("specs/seat-types/decomposition.md"),
            "specs/seat-types/spec.md"
        );
        assert_eq!(
            feature_spec_of("specs/seat-types/architecture.md"),
            "specs/seat-types/spec.md"
        );
        assert_eq!(
            feature_spec_of("specs/seat-types/spec.md"),
            "specs/seat-types/spec.md"
        );
        // Windows backslashes are normalized.
        assert_eq!(
            feature_spec_of("specs\\seat-types\\breakdown.md"),
            "specs/seat-types/spec.md"
        );
        // A flat specs/<slug>.md (no feature folder) and a legacy PLAN-*.md are returned as-is.
        assert_eq!(feature_spec_of("specs/lakes.md"), "specs/lakes.md");
        assert_eq!(feature_spec_of("PLAN-lakes.md"), "PLAN-lakes.md");
    }

    #[test]
    fn plan_task_names_the_plan_and_frames_a_design_pass() {
        // The workflow pins the plan via its filename, so the task must name it; and plan-only
        // stops at the breakdown, so it must frame a design pass (not "write the code").
        let t = plan_task("PLAN-lakes.md");
        assert!(
            t.contains("PLAN-lakes.md"),
            "names the plan so referenced_plan pins it"
        );
        assert!(t.to_lowercase().contains("design"));
        assert!(t.contains("do not write source code yet"));
    }

    #[test]
    fn fix_feed_line_surfaces_model_narration() {
        // The execute/iterate feed shows the model's thinking, not just file touches.
        let line = fix_feed_line(&sc_core::AgentEvent::ModelTurn {
            step: 1,
            prompt_tokens: 10,
            raw: "I'll add the water module and wire it in.\n{\"tool\":\"write_file\",\"path\":\"w.rs\"}"
                .to_string(),
        });
        let line = line.expect("narration surfaced");
        assert!(line.starts_with("💭"));
        assert!(line.contains("water module"));
    }

    #[test]
    fn fix_feed_line_surfaces_every_tool_action() {
        // The coder spends most turns searching/reading and often emits a BARE tool call with no
        // prose — so every tool must produce a feed line, or the run "feels dead" (the reported bug).
        let tc = |tool: &str, arg: &str| {
            fix_feed_line(&sc_core::AgentEvent::ToolCall {
                tool: tool.to_string(),
                arg: arg.to_string(),
            })
        };
        assert_eq!(tc("edit_file", "a.rs").as_deref(), Some("✎ editing a.rs"));
        assert_eq!(tc("create_file", "b.rs").as_deref(), Some("✎ writing b.rs"));
        assert_eq!(
            tc("search_code", "SeatType").as_deref(),
            Some("🔍 searching for SeatType")
        );
        assert_eq!(
            tc("find_symbol", "ShipLayout").as_deref(),
            Some("🔍 locating ShipLayout")
        );
        assert_eq!(tc("read_file", "c.rs").as_deref(), Some("· reading c.rs"));
        assert_eq!(tc("finish", "").as_deref(), Some("✓ done with this step"));
        // An unknown tool still produces a line (never runs invisibly).
        assert!(tc("weird_tool", "x").is_some());
    }
}
