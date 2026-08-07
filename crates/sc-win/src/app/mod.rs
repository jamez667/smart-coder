//! The iced application — thin rendering glue over the tested `sc_win` library.
//!
//! All "what to show / what to run" logic lives in [`crate::view`], [`crate::config`],
//! [`crate::session`], and [`crate::bridge`]; this file only lays those out as
//! widgets, pumps the worker channels on a timer tick, and routes button clicks back
//! to the blocking decision seams. Keep it thin.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, container, row, scrollable, text, text_input, Space};
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
            // The restored project needs detecting, or the Compile button stays dead until the
            // user re-picks the folder (spec 21).
            app.refresh_project_kind();
            // The restored layout is the authority on how many panes the user had.
            app.sync_panes_to_layout();
            // Boot is deferred while the first-run question is open (spec 21): opening a
            // conversation is Assistant-shaped, and doing it before the user has said which mode
            // they want would mean undoing it the moment they answer "Just code".
            // `Message::ChooseMode` finishes this once they have.
            if app.picked_workspace.is_some() && app.cfg.mode_chosen() {
                app.show_welcome();
                if !app.cfg.craft() {
                    app.open_conversation();
                }
            }
            // Remote-mirror mode (Claude-Code-remote style): when SC_REMOTE is set, start a
            // mirror server so a phone can attach to THIS live session — see the chat + agent
            // activity, send chat, approve/deny, stop. Bound to 127.0.0.1 (front it with
            // `tailscale serve`); every request needs the printed per-run token.
            // Refused in Craft mode (spec 21): the mirror exists to bring agent output to a
            // phone and to accept chat/approvals back, so starting it would be a model surface
            // arriving through a side door. Say why rather than failing silently — someone who
            // set SC_REMOTE deliberately deserves to know it was ignored.
            if std::env::var("SC_REMOTE").is_ok() {
                if app.cfg.craft() {
                    eprintln!(
                        "SC_REMOTE ignored: the remote mirror is an agent surface, and this \
                         install is in Craft mode (Settings ▸ General)."
                    );
                } else {
                    app.remote = Some(start_mirror());
                    // Publish the initially-open project so the phone shows it on first connect.
                    app.publish_workspace_to_remote();
                }
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
// `impl App` is split across these submodules (each adds its own impl block):
mod logic_a;
mod logic_b;
mod logic_c;
mod logic_compile;
mod logic_save;
mod update;
mod view_code;
mod view_comply;
mod view_core;
mod view_layout;
pub(crate) use view_layout::{Drag, DragSubject};
mod view_menus;
mod view_panels;

mod helpers;
pub(crate) use helpers::*;

mod pane;
pub(crate) use pane::*;

mod tabs;
pub(crate) use tabs::*;

#[cfg(test)]
mod tests {
    use super::*;
    use sc_win::comply::ComplyModel;
    use sc_win::config::Mode;

    /// An `App` in a known mode.
    ///
    /// `App::default()` reads the developer's real config.json, so a test that cares about mode
    /// must SET it rather than assume — otherwise it passes or fails depending on whose machine
    /// it runs on.
    fn app_in(mode: Mode) -> App {
        let mut app = App::default();
        app.cfg.mode = Some(mode);
        app
    }

    /// Craft mode spawns no health probe.
    ///
    /// This is the mode's whole promise (spec 21): no language model is contacted. The probe is
    /// the one caller that dials out on a TIMER rather than on a user action, and it builds an
    /// `OpenAiBackend` DIRECTLY instead of going through `UiConfig::backend()` — so a
    /// mode-aware builder would miss it entirely. Asserting on the spawn, not the builder, is
    /// what keeps that hole closed.
    #[test]
    fn craft_mode_never_spawns_the_health_probe() {
        let mut app = app_in(Mode::Craft);
        app.health_rx = None;
        app.last_health_probe = None; // "never probed" ⇒ a probe is due

        app.tick_health_probe();

        assert!(
            app.health_rx.is_none(),
            "Craft mode must not put a probe in flight"
        );
        assert!(
            app.last_health_probe.is_none(),
            "and must not record having probed"
        );
    }

    /// The same tick DOES probe in Assistant mode — otherwise the test above would pass for the
    /// wrong reason (e.g. a probe that never fires in either mode).
    // A craft-only build has one mode, so there is no switch to exercise.
    #[cfg(not(feature = "craft-only"))]
    #[test]
    fn assistant_mode_still_spawns_the_health_probe() {
        let mut app = app_in(Mode::Assistant);
        app.health_rx = None;
        app.last_health_probe = None;

        app.tick_health_probe();

        assert!(app.health_rx.is_some(), "Assistant mode probes as before");
    }

    /// Switching to Craft clears the backend verdict.
    ///
    /// A stale "backend reachable" badge left on screen in a mode that contacts no backend is a
    /// lie about the thing the user just asked us to stop doing.
    // A craft-only build has one mode, so there is no switch to exercise.
    #[cfg(not(feature = "craft-only"))]
    #[test]
    fn entering_craft_mode_clears_the_backend_health_verdict() {
        let mut app = app_in(Mode::Assistant);
        app.backend_health = Some(sc_model::BackendHealth::Ready);

        let _ = app.update(Message::ToggleCraftMode(true));

        assert!(app.cfg.craft(), "mode switched");
        assert!(app.backend_health.is_none(), "stale verdict dropped");
        assert!(app.health_rx.is_none(), "no probe left in flight");
    }

    /// Chat send is refused in Craft mode — at the entry point, not just in the view.
    ///
    /// Hiding the composer is presentation. This asserts the REFUSAL: a keyboard shortcut, a
    /// replayed message, or a caller added later must not be able to spawn a model turn.
    #[test]
    fn craft_mode_refuses_to_send_chat() {
        let mut app = app_in(Mode::Craft);
        app.intent = "write me a parser".to_string();

        app.send_chat();

        assert!(app.chat_session.is_none(), "no model turn may be spawned");
        assert_eq!(app.intent, "write me a parser", "composer left untouched");
    }

    /// Same for starting a run.
    #[test]
    fn craft_mode_refuses_to_start_a_run() {
        let mut app = app_in(Mode::Craft);
        app.intent = "build the thing".to_string();

        app.start(RunKind::Agent);

        assert!(app.session.is_none(), "no run may be spawned");
    }

    /// A line comment still SAVES in Craft mode; only the auto-fix stops.
    ///
    /// The distinction matters: line comments are a review annotation, not an AI feature — they
    /// are also how Send back harvests revision notes. A naive "hide the AI bits" pass would
    /// break the annotation along with the model call.
    #[test]
    fn craft_mode_keeps_line_comments_but_never_triages_them() {
        let mut app = app_in(Mode::Craft);
        app.panes.focused_mut().code = Some(sc_win::codeview::CodeView {
            rel: "src/main.rs".to_string(),
            lines: vec![(1, "fn main() {}".to_string())],
            truncated: false,
            note: None,
        });
        app.panes.focused_mut().comment_range = Some((1, 1));
        app.panes.focused_mut().comment_draft = "this allocates twice".to_string();

        app.submit_line_comment();

        assert!(app.triage.is_none(), "no triage call may be spawned");
        assert!(app.working.is_none(), "and no agent-working range is set");
        assert!(
            app.comments
                .on_file("src/main.rs")
                .any(|(_, c)| c.text == "this allocates twice"),
            "but the comment itself is kept"
        );
        assert!(
            app.panes.focused_mut().comment_range.is_none(),
            "and the box closes"
        );
    }

    /// Clicking a diagnostic opens its file and queues the scroll to its line.
    ///
    /// This is what makes the Problems panel a list rather than a log. If it stops working the
    /// feature degrades to a wall of text the terminal already provides.
    #[test]
    fn clicking_a_problem_opens_the_file_at_that_line() {
        let (mut app, dir) = app_with_file("Broken.cs", "class A {\n  int x =\n}\n");
        app.compile_report = Some(sc_win::diagnostics::CompileReport {
            diagnostics: vec![sc_win::diagnostics::Diagnostic {
                file: "Broken.cs".to_string(),
                line: 2,
                col: 9,
                severity: sc_win::diagnostics::Severity::Error,
                code: Some("CS1525".to_string()),
                message: "invalid expression term".to_string(),
            }],
            exit_code: Some(1),
            failure: None,
        });

        let _ = app.open_diagnostic(0);

        assert_eq!(
            app.panes.focused_mut().selected_file.as_deref(),
            Some("Broken.cs"),
            "opened"
        );
        assert_eq!(
            app.panes.focused_mut().pending_scroll_line,
            Some(2),
            "and queued the jump"
        );
        assert!(!app.follow_agent, "clicking a problem pins the view");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A diagnostic pointing outside the workspace opens nothing.
    ///
    /// Compilers report paths into package caches and SDK sources. Opening those gives a tab
    /// full of "(file not found)", which is worse than the click doing nothing.
    #[test]
    fn a_problem_outside_the_workspace_is_not_opened() {
        let (mut app, dir) = app_with_file("Real.cs", "class A {}\n");
        app.compile_report = Some(sc_win::diagnostics::CompileReport {
            diagnostics: vec![sc_win::diagnostics::Diagnostic {
                file: "C:/Program Files/Unity/Editor/Data/Managed/UnityEngine.dll".to_string(),
                line: 1,
                col: 1,
                severity: sc_win::diagnostics::Severity::Error,
                code: None,
                message: "somewhere else entirely".to_string(),
            }],
            exit_code: Some(1),
            failure: None,
        });

        let _ = app.open_diagnostic(0);

        assert!(
            app.panes.focused_mut().selected_file.is_none(),
            "nothing opened"
        );
        assert!(app.panes.focused_mut().pending_scroll_line.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// An unrecognised project reports why rather than compiling something arbitrary.
    #[test]
    fn compiling_an_unrecognised_project_explains_itself() {
        let (mut app, dir) = app_with_file("notes.txt", "just some notes\n");
        app.refresh_project_kind();
        assert_eq!(app.project_kind, sc_win::project::ProjectKind::Unknown);

        let _ = app.start_compile();

        assert!(!app.compiling, "nothing was launched");
        let report = app.compile_report.as_ref().expect("reported");
        let failure = report.failure.as_deref().unwrap_or_default();
        assert!(
            failure.contains("Unity"),
            "names what it looked for: {failure}"
        );
        assert!(!report.ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Switching projects drops the previous one's problems.
    ///
    /// Stale diagnostics against a different project are worse than none — they point at files
    /// that may not exist and imply a build that never ran.
    #[test]
    fn changing_project_clears_stale_problems() {
        let (mut app, dir) = app_with_file("a.rs", "fn main() {}\n");
        app.compile_report = Some(sc_win::diagnostics::CompileReport {
            diagnostics: vec![],
            exit_code: Some(1),
            failure: Some("from the last project".to_string()),
        });

        app.refresh_project_kind();

        assert!(app.compile_report.is_none(), "stale report dropped");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Point layout persistence at a scratch dir for the duration of a test.
    ///
    /// `App::default()` reads and writes REAL machine state, so a test that toggles a panel
    /// otherwise rewrites the developer's actual `layout.json` — which it did, once, before this
    /// existed. Tests that mutate the layout must call this first.
    fn redirect_layout_state(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-win-layout-{name}-{:?}",
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: single-threaded test process state; each test uses its own directory.
        unsafe { std::env::set_var("SC_STATE_DIR", &dir) };
        dir
    }

    /// Switching modes swaps the layout, and each mode keeps its own arrangement.
    ///
    /// A shared tree would mean entering Craft mode silently rearranged the Assistant one — the
    /// user would come back to find their panels moved by a setting that was supposed to be
    /// reversible.
    // A craft-only build has one mode, so there is no switch to exercise.
    #[cfg(not(feature = "craft-only"))]
    #[test]
    fn each_mode_keeps_its_own_panel_arrangement() {
        use sc_win::layout::{EditorId, PanelKind};
        let dir = redirect_layout_state("modes");
        let mut app = app_in(Mode::Assistant);
        app.layout = sc_win::layout::Layout::assistant_default();

        // Rearrange Assistant: drop the git panel.
        let _ = app.update(Message::TogglePanel(PanelKind::Git));
        assert!(!app.layout.contains(PanelKind::Git), "hidden");
        assert!(app.layout.contains(PanelKind::Chat), "still Assistant");

        // Into Craft: chat is gone, and this is a DIFFERENT arrangement.
        let _ = app.update(Message::ToggleCraftMode(true));
        assert!(
            !app.layout.contains(PanelKind::Chat),
            "no chat without a model"
        );
        assert!(
            app.layout.contains(PanelKind::Editor(EditorId::FIRST)),
            "editor survives"
        );

        // Back to Assistant: the arrangement we left is restored, git still hidden.
        let _ = app.update(Message::ToggleCraftMode(false));
        assert!(app.layout.contains(PanelKind::Chat), "chat is back");
        assert!(
            !app.layout.contains(PanelKind::Git),
            "and OUR arrangement survived the round trip"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Splitting with one file open gives a second, empty pane — the tab stays put.
    ///
    /// Moving a pane's ONLY tab would empty it, and `prune_empty_panes` would then close it,
    /// collapsing straight back to one pane. So the tab stays and the new pane is somewhere to
    /// open the next file. Showing the same file in both is not available: one path, one buffer.
    #[test]
    fn splitting_a_single_tab_pane_leaves_the_tab_and_adds_an_empty_pane() {
        use sc_win::layout::{EditorId, PanelKind};
        let dir = redirect_layout_state("split-editor");
        let (mut app, ws) = app_with_file("split.rs", "fn split() {}\n");
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("split.rs".to_string());
        assert_eq!(app.panes.len(), 1);

        let _ = app.update(Message::SplitEditor);

        assert_eq!(app.panes.len(), 2, "a second pane exists");
        assert_eq!(
            app.layout.editor_ids().len(),
            2,
            "and the layout renders both"
        );
        // Exactly one pane holds it — never two buffers over one path.
        let holders: Vec<_> = app
            .panes
            .iter()
            .filter(|(_, p)| p.holds("split.rs"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            holders,
            vec![EditorId::FIRST],
            "the tab stayed where it was"
        );
        assert_ne!(
            app.panes.focused_id(),
            EditorId::FIRST,
            "focus moved to the new pane — you split to work over there"
        );
        assert!(app.layout.contains(PanelKind::Editor(EditorId::FIRST)));

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// With two tabs open, splitting MOVES the active one into the new pane.
    #[test]
    fn splitting_a_multi_tab_pane_moves_the_active_tab_across() {
        use sc_win::layout::EditorId;
        let dir = redirect_layout_state("split-move");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        std::fs::write(ws.join("b.rs"), "fn b() {}\n").unwrap();
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("a.rs".to_string());
        app.select_file("b.rs".to_string()); // b is now active, both open
        assert_eq!(app.panes.focused().tabs.len(), 2);

        let _ = app.update(Message::SplitEditor);

        assert_eq!(app.panes.len(), 2);
        // The ACTIVE tab moved; the other stayed.
        assert!(
            app.panes
                .get(EditorId::FIRST)
                .is_some_and(|p| p.holds("a.rs")),
            "the inactive tab stayed behind"
        );
        let moved: Vec<_> = app
            .panes
            .iter()
            .filter(|(_, p)| p.holds("b.rs"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(moved.len(), 1, "b.rs is open in exactly one pane");
        assert_ne!(moved[0], EditorId::FIRST, "and it moved to the new one");
        assert_eq!(app.panes.focused_id(), moved[0], "focus followed it");

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A press that never moves is a CLICK: it selects, and creates no drag.
    ///
    /// Selection had to move from press to release so the same gesture could also start a drag.
    /// Without the threshold every click would be a one-pixel drag and the strip would flicker.
    #[test]
    fn a_tab_press_without_movement_selects_and_never_drags() {
        use sc_win::layout::EditorId;
        let dir = redirect_layout_state("tab-click");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        std::fs::write(ws.join("b.rs"), "fn b() {}\n").unwrap();
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("a.rs".to_string());
        app.select_file("b.rs".to_string()); // b active

        // Press a.rs's tab — nothing selected yet, and the drag is not armed.
        let _ = app.update(Message::TabPress(EditorId::FIRST, "a.rs".to_string()));
        assert_eq!(
            app.panes.focused().selected_file.as_deref(),
            Some("b.rs"),
            "press alone does not select — that would flicker at the start of every drag"
        );
        assert!(!app.dragging(), "and it is not yet a drag");

        // Release without having moved.
        let _ = app.update(Message::TabRelease("a.rs".to_string()));
        assert_eq!(
            app.panes.focused().selected_file.as_deref(),
            Some("a.rs"),
            "release-without-movement is a click, so it selects"
        );
        assert!(app.drag.is_none(), "and leaves nothing in flight");
        assert_eq!(app.panes.len(), 1, "no pane was created");

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A press that moves past the threshold arms the drag — and only then.
    #[test]
    fn a_tab_drag_arms_only_after_the_threshold() {
        use sc_win::layout::EditorId;
        let dir = redirect_layout_state("tab-threshold");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        app.select_file("a.rs".to_string());

        app.cursor_pos = iced::Point::new(100.0, 100.0);
        let _ = app.update(Message::TabPress(EditorId::FIRST, "a.rs".to_string()));

        // A jitter of a pixel or two is a click, not a drag.
        let _ = app.update(Message::GitCursorMoved(iced::Point::new(101.0, 101.0)));
        assert!(!app.dragging(), "1.4px of jitter is still a click");

        let _ = app.update(Message::GitCursorMoved(iced::Point::new(120.0, 100.0)));
        assert!(app.dragging(), "20px is unmistakably a drag");

        // Latched: wandering back over the origin does not un-arm it.
        let _ = app.update(Message::GitCursorMoved(iced::Point::new(100.0, 100.0)));
        assert!(
            app.dragging(),
            "a drag that passes back over its origin stays a drag"
        );

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **The feature.** Dragging a tab to a pane edge opens a NEW pane holding that tab.
    #[test]
    fn dragging_a_tab_to_an_edge_opens_a_new_pane_for_it() {
        use sc_win::layout::{EditorId, PanelKind, Side};
        let dir = redirect_layout_state("tab-drag-out");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        std::fs::write(ws.join("b.rs"), "fn b() {}\n").unwrap();
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("a.rs".to_string());
        app.select_file("b.rs".to_string());
        assert_eq!(app.panes.len(), 1);

        app.cursor_pos = iced::Point::new(100.0, 100.0);
        let _ = app.update(Message::TabPress(EditorId::FIRST, "b.rs".to_string()));
        let _ = app.update(Message::GitCursorMoved(iced::Point::new(400.0, 100.0)));
        // Aim at the right edge of the editor pane.
        let _ = app.update(Message::PanelHover(
            PanelKind::Editor(EditorId::FIRST),
            95.0,
            50.0,
            100.0,
            100.0,
            2000.0,
            1000.0,
        ));
        assert_eq!(
            app.drop_target.map(|(_, s, _)| s),
            Some(Side::Right),
            "a tab drag lights up pane edges, same as a panel drag"
        );
        let _ = app.update(Message::PanelDrop);

        assert_eq!(app.panes.len(), 2, "a new pane opened for the dragged tab");
        assert_eq!(
            app.layout.editor_ids().len(),
            2,
            "and the layout renders it"
        );
        let holders: Vec<_> = app
            .panes
            .iter()
            .filter(|(_, p)| p.holds("b.rs"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(holders.len(), 1, "one path, one buffer — never copied");
        assert_ne!(holders[0], EditorId::FIRST, "it moved to the new pane");
        assert!(
            app.panes
                .get(EditorId::FIRST)
                .is_some_and(|p| p.holds("a.rs")),
            "and the tab left behind stayed"
        );
        assert!(app.drag.is_none(), "the drag ended");

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **The round trip.** Dragging that tab back onto the other pane's strip closes the pane it
    /// left behind — which is the gesture as the user described it.
    #[test]
    fn dragging_the_last_tab_back_closes_the_pane_it_emptied() {
        use sc_win::layout::EditorId;
        let dir = redirect_layout_state("tab-drag-back");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        std::fs::write(ws.join("b.rs"), "fn b() {}\n").unwrap();
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("a.rs".to_string());
        app.select_file("b.rs".to_string());

        // Split b.rs out into its own pane.
        let _ = app.update(Message::SplitEditor);
        let second = app.panes.focused_id();
        assert_ne!(second, EditorId::FIRST);
        assert_eq!(app.panes.len(), 2);

        // Now drag it back onto pane 0's strip.
        app.cursor_pos = iced::Point::new(400.0, 100.0);
        let _ = app.update(Message::TabPress(second, "b.rs".to_string()));
        let _ = app.update(Message::GitCursorMoved(iced::Point::new(100.0, 100.0)));
        assert!(app.dragging());
        let _ = app.update(Message::TabDropOnPane(EditorId::FIRST));

        assert_eq!(
            app.panes.len(),
            1,
            "the pane it emptied closed itself — drag out to open, drag back to close"
        );
        assert_eq!(app.layout.editor_ids(), vec![EditorId::FIRST]);
        assert!(app.panes.focused().holds("a.rs") && app.panes.focused().holds("b.rs"));
        assert!(app.drag.is_none());

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A tab dropped back on the pane it came from changes nothing.
    ///
    /// The no-op has to be explicit: the move is a remove-then-add, so treating this as a real
    /// drop would take the tab out of its only pane and rely on the re-add to save it.
    #[test]
    fn dropping_a_tab_back_on_its_own_pane_is_a_no_op() {
        use sc_win::layout::EditorId;
        let dir = redirect_layout_state("tab-self-drop");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("a.rs".to_string());
        let before = app.layout.clone();

        app.cursor_pos = iced::Point::new(100.0, 100.0);
        let _ = app.update(Message::TabPress(EditorId::FIRST, "a.rs".to_string()));
        let _ = app.update(Message::GitCursorMoved(iced::Point::new(150.0, 100.0)));
        let _ = app.update(Message::TabDropOnPane(EditorId::FIRST));

        assert_eq!(app.panes.len(), 1, "no pane appeared or vanished");
        assert_eq!(app.layout, before, "and the layout is untouched");
        assert!(app.panes.focused().holds("a.rs"), "the tab is still open");
        assert!(app.drag.is_none());

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Dragging a lone tab to its OWN pane's edge is a no-op, not a split into an empty half.
    #[test]
    fn dragging_a_lone_tab_to_its_own_edge_does_nothing() {
        use sc_win::layout::{EditorId, PanelKind, Side};
        let dir = redirect_layout_state("tab-lone-edge");
        let (mut app, ws) = app_with_file("a.rs", "fn a() {}\n");
        app.layout = sc_win::layout::Layout::craft_default();
        app.select_file("a.rs".to_string());
        let before = app.layout.clone();

        app.cursor_pos = iced::Point::new(100.0, 100.0);
        let _ = app.update(Message::TabPress(EditorId::FIRST, "a.rs".to_string()));
        let _ = app.update(Message::GitCursorMoved(iced::Point::new(150.0, 100.0)));
        app.drop_target = Some((PanelKind::Editor(EditorId::FIRST), Side::Right, false));
        let _ = app.update(Message::PanelDrop);

        assert_eq!(
            app.panes.len(),
            1,
            "nothing to separate, so nothing happened"
        );
        assert_eq!(app.layout, before);
        assert!(app.panes.focused().holds("a.rs"));

        let _ = std::fs::remove_dir_all(ws);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Splitting with nothing open is a no-op.
    ///
    /// The new pane carries the active tab, so with no tab it would be an empty pane beside an
    /// empty pane — a smaller editor and nothing else.
    #[test]
    fn splitting_with_no_file_open_does_nothing() {
        let dir = redirect_layout_state("split-empty");
        let mut app = app_in(Mode::Craft);
        app.layout = sc_win::layout::Layout::craft_default();

        let _ = app.update(Message::SplitEditor);

        assert_eq!(app.panes.len(), 1, "no pane was created");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Keystrokes reach the pane that sent them, not whichever pane holds focus.
    ///
    /// THE correctness bug multi-pane introduces. `EditorEvent` used to route through
    /// `active_tab_mut()`, which reads the FOCUSED pane — so with two live editors, a keystroke
    /// delivered in the same batch as the click that moved focus lands in the wrong file. Silent,
    /// and it corrupts the file you weren't looking at.
    #[test]
    fn keystrokes_go_to_the_pane_that_sent_them() {
        use sc_win::layout::EditorId;
        let (mut app, dir) = app_with_file("a.rs", "fn a() {}\n");
        std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();

        // Pane 0 holds a.rs; a second pane holds b.rs.
        app.select_file("a.rs".to_string());
        let second = app.panes.insert();
        app.select_file_into("b.rs".to_string(), Origin::Tree, second);

        // Focus pane 0 — the WRONG pane for the event we're about to send.
        app.panes.focus(EditorId::FIRST);
        assert_eq!(app.panes.focused_id(), EditorId::FIRST);

        let before_a = app
            .panes
            .get(EditorId::FIRST)
            .and_then(|p| p.active_tab())
            .and_then(|t| t.text());
        // Type into the SECOND pane while the first is focused.
        let _ = app.update(Message::EditorEvent(
            second,
            iced_code_editor::Message::CharacterInput('X'),
        ));

        assert_eq!(
            app.panes
                .get(EditorId::FIRST)
                .and_then(|p| p.active_tab())
                .and_then(|t| t.text()),
            before_a,
            "the focused pane's buffer must be untouched"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file already open in one pane is focused there, not opened twice.
    ///
    /// `Tab` owns its buffer, so a second copy would be a second independent buffer over one
    /// path — and the save-conflict slot is path-keyed, so saving one would raise a bogus
    /// conflict in the other whose Overwrite destroys the first's edits.
    #[test]
    fn opening_a_file_already_open_elsewhere_focuses_that_pane() {
        use sc_win::layout::EditorId;
        let (mut app, dir) = app_with_file("shared.rs", "fn shared() {}\n");

        app.select_file("shared.rs".to_string());
        let second = app.panes.insert();

        // Ask for it in the OTHER pane; it should take us back to the one that has it.
        app.select_file_into("shared.rs".to_string(), Origin::Tree, second);

        assert_eq!(
            app.panes.focused_id(),
            EditorId::FIRST,
            "focus followed the file rather than opening a copy"
        );
        assert_eq!(
            app.panes.get(second).map(|p| p.tabs.len()),
            Some(0),
            "and the other pane did not gain a second buffer for it"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Unsaved work in a BACKGROUND pane still blocks quit.
    ///
    /// `any_dirty` drives the window-title dot and the quit prompt. Scanning only the focused
    /// pane would lose exactly the case the prompt exists for.
    #[test]
    fn an_unsaved_buffer_in_a_background_pane_still_blocks_quit() {
        use sc_win::layout::EditorId;
        let (mut app, dir) = app_with_file("bg.rs", "fn bg() {}\n");

        let second = app.panes.insert();
        app.select_file_into("bg.rs".to_string(), Origin::Tree, second);
        if let Some(t) = app.panes.get_mut(second).and_then(|p| p.active_tab_mut()) {
            t.dirty = true;
        }
        // Look away.
        app.panes.focus(EditorId::FIRST);

        assert!(
            app.any_dirty(),
            "a dirty buffer in a pane you aren't looking at still counts"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Dragging a panel's header onto another panel's edge rearranges the layout.
    #[test]
    fn dropping_a_panel_on_another_rearranges_and_persists() {
        use sc_win::layout::{EditorId, PanelKind};
        let dir = redirect_layout_state("panel-drop");
        let mut app = app_in(Mode::Craft);
        app.layout = sc_win::layout::Layout::craft_default();
        let before = app.layout.clone();

        // Grab Git's header, hover the right edge of the editor, release.
        let _ = app.update(Message::PanelGrab(PanelKind::Git));
        assert_eq!(app.dragged_panel(), Some(PanelKind::Git), "picked up");
        // A small panel in the middle of a large window, so its right edge is INTERIOR — the
        // drop splits the editor rather than spanning the whole layout.
        app.window_w = 2000.0;
        app.window_h = 1000.0;
        app.cursor_pos = iced::Point::new(500.0, 300.0);
        let _ = app.update(Message::PanelHover(
            PanelKind::Editor(EditorId::FIRST),
            95.0,
            50.0,
            100.0,
            100.0,
            2000.0,
            1000.0,
        ));
        assert_eq!(
            app.drop_target,
            Some((
                PanelKind::Editor(EditorId::FIRST),
                sc_win::layout::Side::Right,
                false
            )),
            "the near edge decides the side, and this one is interior"
        );
        let _ = app.update(Message::PanelDrop);

        assert_ne!(app.layout, before, "the layout changed");
        assert!(app.drag.is_none(), "the drag ended");
        assert!(app.drop_target.is_none());
        // Membership is unchanged — a move must never lose or duplicate a panel.
        let (mut a, mut b) = (before.panels(), app.layout.panels());
        a.sort();
        b.sort();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Dropping at the layout's BOTTOM edge makes a full-width row.
    ///
    /// The reported bug: bottom docking never fired. The tree renders below the menu bar, so the
    /// bottom panel's edge is ~34px above `window_h` — testing against the window instead of the
    /// tree meant the condition could never be true. This models the real geometry.
    #[test]
    fn dropping_at_the_bottom_edge_docks_across_the_full_width() {
        use sc_win::layout::{Axis, EditorId, Layout, PanelKind, Side};
        let dir = redirect_layout_state("panel-bottom-drop");
        let mut app = app_in(Mode::Craft);
        app.layout = Layout::craft_default();

        // A 1000x800 window with a 34px menu bar → the tree is 1000x766, well short of the
        // window's height. That gap is what the old window-relative test could never bridge.
        const MENU_BAR: f32 = 34.0;
        app.window_w = 1000.0;
        app.window_h = 800.0;
        let (tw, th) = (1000.0, 800.0 - MENU_BAR);

        // The Editor spanning the tree's full WIDTH, cursor near its lower edge.
        let (w, h) = (tw, 383.0);
        let (x, y) = (500.0, 378.0);

        let _ = app.update(Message::PanelGrab(PanelKind::Git));
        let _ = app.update(Message::PanelHover(
            PanelKind::Editor(EditorId::FIRST),
            x,
            y,
            w,
            h,
            tw,
            th,
        ));

        assert_eq!(
            app.drop_target,
            Some((PanelKind::Editor(EditorId::FIRST), Side::Bottom, true)),
            "the tree's bottom edge must register as OUTER even though it isn't the window's"
        );

        let _ = app.update(Message::PanelDrop);

        // Git is now the root's lower child — a full-width row under everything else.
        match &app.layout {
            Layout::Split { axis, b, .. } => {
                assert_eq!(*axis, Axis::Vertical, "a row");
                assert_eq!(**b, Layout::Leaf(PanelKind::Git), "spanning the bottom");
            }
            other => panic!("expected a root split, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Dropping on the WINDOW's dock frame docks across the whole layout.
    ///
    /// The frame is a separate surface from the per-panel zones: it hugs the outside of the
    /// entire tree, so "throw it at the edge" works without aiming at any particular panel.
    #[test]
    fn dropping_on_the_window_dock_frame_spans_the_layout() {
        use sc_win::layout::{Axis, Layout, PanelKind, Side};
        let dir = redirect_layout_state("dock-frame");
        let mut app = app_in(Mode::Craft);
        app.layout = Layout::craft_default();

        let _ = app.update(Message::PanelGrab(PanelKind::Git));
        let _ = app.update(Message::DockHover(Some(Side::Bottom)));
        assert_eq!(app.dock_side, Some(Side::Bottom));

        let _ = app.update(Message::PanelDrop);

        match &app.layout {
            Layout::Split { axis, b, .. } => {
                assert_eq!(*axis, Axis::Vertical, "a full-width row");
                assert_eq!(**b, Layout::Leaf(PanelKind::Git), "docked at the bottom");
            }
            other => panic!("expected a root split, got {other:?}"),
        }
        assert!(app.dock_side.is_none(), "drag state cleared");
        assert!(app.drag.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The window frame wins over whatever panel is underneath it.
    ///
    /// The bands sit ABOVE the panels, so pointing at one means "dock across everything" — the
    /// per-panel target must not also be live, or the highlight would promise two outcomes.
    #[test]
    fn the_dock_frame_supersedes_a_per_panel_target() {
        use sc_win::layout::{EditorId, Layout, PanelKind, Side};
        let dir = redirect_layout_state("dock-priority");
        let mut app = app_in(Mode::Craft);
        app.layout = Layout::craft_default();

        let _ = app.update(Message::PanelGrab(PanelKind::Git));
        // A per-panel target first…
        let _ = app.update(Message::PanelHover(
            PanelKind::Editor(EditorId::FIRST),
            500.0,
            300.0,
            1000.0,
            600.0,
            1000.0,
            766.0,
        ));
        assert!(app.drop_target.is_some());

        // …then the cursor reaches the frame, which takes over.
        let _ = app.update(Message::DockHover(Some(Side::Left)));
        assert_eq!(app.dock_side, Some(Side::Left));
        assert!(
            app.drop_target.is_none(),
            "the per-panel target must clear, so only one outcome is promised"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Releasing away from any panel cancels the drag instead of stranding it.
    ///
    /// Without this, letting go over the menu bar (or off the window) would leave a panel
    /// permanently "in flight" and every other panel showing drop zones.
    #[test]
    fn releasing_outside_a_drop_target_cancels_the_drag() {
        use sc_win::layout::PanelKind;
        let dir = redirect_layout_state("panel-cancel");
        let mut app = app_in(Mode::Craft);
        app.layout = sc_win::layout::Layout::craft_default();
        let before = app.layout.clone();

        let _ = app.update(Message::PanelGrab(PanelKind::Git));
        // No hover — the release happens somewhere that isn't a panel.
        let _ = app.update(Message::SplitDragEnd);

        assert!(app.drag.is_none(), "drag cancelled");
        assert_eq!(app.layout, before, "and nothing moved");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The editor can never be hidden.
    ///
    /// An IDE with nothing to edit isn't a layout choice, it's a broken window — so the toggle
    /// refuses rather than leaving the user with no way back.
    #[test]
    fn the_editor_panel_cannot_be_hidden() {
        use sc_win::layout::{EditorId, PanelKind};
        let dir = redirect_layout_state("no-hide-editor");
        let mut app = app_in(Mode::Craft);
        app.layout = sc_win::layout::Layout::craft_default();

        let _ = app.update(Message::TogglePanel(PanelKind::Editor(EditorId::FIRST)));

        assert!(
            app.layout.contains(PanelKind::Editor(EditorId::FIRST)),
            "hiding the editor must be refused"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The first-run question shows only while no mode has been chosen.
    ///
    /// The tri-state is the point: "never asked" is a different state from "chose Assistant",
    /// and collapsing them would either nag someone who already answered or never ask at all.
    // A craft-only build has one mode, so there is no switch to exercise.
    #[cfg(not(feature = "craft-only"))]
    #[test]
    fn the_first_run_question_shows_once_and_only_when_unanswered() {
        let mut app = App::default();

        app.cfg.mode = None;
        assert!(
            app.view_first_run().is_some(),
            "unanswered → the question is asked"
        );

        for m in [Mode::Craft, Mode::Assistant] {
            app.cfg.mode = Some(m);
            assert!(
                app.view_first_run().is_none(),
                "{} was chosen → never asked again",
                m.slug()
            );
        }
    }

    /// Answering the question records the mode.
    // A craft-only build has one mode, so there is no switch to exercise.
    #[cfg(not(feature = "craft-only"))]
    #[test]
    fn choosing_a_mode_answers_the_question_for_good() {
        for (craft, expected) in [(true, Mode::Craft), (false, Mode::Assistant)] {
            let mut app = App::default();
            app.cfg.mode = None;

            let _ = app.update(Message::ChooseMode(craft));

            assert_eq!(app.cfg.mode, Some(expected));
            assert!(app.cfg.mode_chosen(), "and it counts as answered");
            assert!(app.view_first_run().is_none(), "so the prompt is gone");
        }
    }

    /// Escape only means anything while the question is open.
    ///
    /// It declines (and so quits) rather than picking — being chosen for is precisely what this
    /// feature avoids. Once a mode exists, Escape must not be repurposed silently.
    #[test]
    fn escape_declines_the_question_but_is_inert_afterwards() {
        let mut app = App::default();
        app.cfg.mode = Some(Mode::Assistant);
        let _ = app.update(Message::EscapePressed);
        assert_eq!(app.cfg.mode, Some(Mode::Assistant), "nothing changed");

        // Unanswered: Escape must NOT write a mode. Declining is not an answer, so the question
        // returns on the next launch.
        let mut app = App::default();
        app.cfg.mode = None;
        let _ = app.update(Message::EscapePressed);
        assert_eq!(app.cfg.mode, None, "declining never picks a mode");
    }

    /// A scratch workspace with one file in it, and an `App` pointed at it.
    fn app_with_file(name: &str, body: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sc-win-edit-{name}-{:?}",
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(name), body).unwrap();
        let mut app = App::default();
        app.picked_workspace = Some(dir.clone());
        (app, dir)
    }

    /// Opening a file from the tree gives an editable buffer; opening the same file again
    /// re-selects the tab rather than rebuilding it.
    ///
    /// The re-select path is what protects unsaved edits: rebuilding would silently discard the
    /// buffer, which is how a click on an already-open tab could eat your typing.
    #[test]
    fn reopening_a_tab_reuses_its_buffer_instead_of_rebuilding() {
        let (mut app, dir) = app_with_file("a.rs", "fn main() {}\n");

        app.select_file("a.rs".to_string());
        assert_eq!(app.panes.focused_mut().tabs.len(), 1);
        assert!(
            app.panes.focused_mut().tabs[0].editable(),
            "a normal source file is editable"
        );
        assert_eq!(
            app.panes.focused_mut().tabs[0].view,
            TabView::Edit,
            "tree opens for editing"
        );

        // Mark it dirty, then "re-open" it the way a tab click does.
        app.panes.focused_mut().tabs[0].dirty = true;
        app.select_file("a.rs".to_string());

        assert_eq!(app.panes.focused_mut().tabs.len(), 1, "no duplicate tab");
        assert!(
            app.panes.focused_mut().tabs[0].dirty,
            "the buffer survived the re-select"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The git panel opens files for REVIEW, not editing.
    #[test]
    fn the_git_panel_opens_files_in_the_review_view() {
        let (mut app, dir) = app_with_file("b.rs", "fn main() {}\n");
        app.select_file_for_review("b.rs".to_string());
        assert_eq!(app.panes.focused_mut().tabs[0].view, TabView::Review);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Closing a tab with unsaved edits prompts instead of closing.
    ///
    /// The whole point: a stray click on ✕ must not be able to discard work.
    #[test]
    fn closing_a_dirty_tab_asks_first() {
        let (mut app, dir) = app_with_file("c.rs", "fn main() {}\n");
        app.select_file("c.rs".to_string());
        app.panes.focused_mut().tabs[0].dirty = true;

        app.close_tab("c.rs");
        assert_eq!(app.panes.focused_mut().tabs.len(), 1, "still open");
        assert_eq!(app.confirm_close.as_deref(), Some("c.rs"), "prompting");

        // Discarding is the explicit answer, and only then does it close.
        let _ = app.update(Message::DiscardAndClose("c.rs".to_string()));
        assert!(
            app.panes.focused_mut().tabs.is_empty(),
            "closed on an explicit discard"
        );
        assert!(app.confirm_close.is_none(), "prompt dismissed");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A clean tab closes immediately — the prompt is only for unsaved work.
    #[test]
    fn closing_a_clean_tab_does_not_ask() {
        let (mut app, dir) = app_with_file("d.rs", "fn main() {}\n");
        app.select_file("d.rs".to_string());
        app.close_tab("d.rs");
        assert!(app.panes.focused_mut().tabs.is_empty());
        assert!(app.confirm_close.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Saving writes the buffer and clears the dirty flag.
    #[test]
    fn saving_writes_the_buffer_to_disk() {
        let (mut app, dir) = app_with_file("e.rs", "original\n");
        app.select_file("e.rs".to_string());

        // Stand in for typing. The widget only mutates through input events (there is no
        // setter), so a test substitutes a buffer with the post-edit contents — what's under
        // test here is the SAVE path, not the widget's own key handling.
        app.panes.focused_mut().tabs[0].buf = Buffer::Live(Box::new(
            iced_code_editor::CodeEditor::new("edited\n", "rs"),
        ));
        app.panes.focused_mut().tabs[0].dirty = true;

        app.save_active_tab(false);

        assert_eq!(
            std::fs::read_to_string(dir.join("e.rs")).unwrap(),
            "edited\n"
        );
        assert!(
            !app.panes.focused_mut().tabs[0].dirty,
            "clean after a successful save"
        );
        assert!(app.save_conflict.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A save keeps the file's trailing newline.
    ///
    /// The editor widget's `content()` rejoins its lines and DROPS a trailing newline. Almost
    /// every source file has one, so without the fix in `save_tab` every single save would
    /// rewrite the last line of every file touched — a spurious diff on work you didn't do.
    /// Found by the save test failing on `"edited"` vs `"edited\n"`.
    #[test]
    fn saving_preserves_the_trailing_newline() {
        let (mut app, dir) = app_with_file("nl.rs", "fn main() {}\n");
        app.select_file("nl.rs".to_string());
        assert!(
            app.panes.focused_mut().tabs[0].trailing_newline,
            "recorded at open"
        );
        app.panes.focused_mut().tabs[0].buf = Buffer::Live(Box::new(
            iced_code_editor::CodeEditor::new("fn main() { /* edited */ }\n", "rs"),
        ));
        app.panes.focused_mut().tabs[0].dirty = true;

        app.save_active_tab(false);

        let on_disk = std::fs::read_to_string(dir.join("nl.rs")).unwrap();
        assert!(on_disk.ends_with('\n'), "newline restored: {on_disk:?}");

        // And a file WITHOUT one does not gain one.
        std::fs::write(dir.join("no-nl.rs"), "no newline").unwrap();
        app.select_file("no-nl.rs".to_string());
        let i = app
            .panes
            .focused_mut()
            .tabs
            .iter()
            .position(|t| t.path == "no-nl.rs")
            .unwrap();
        assert!(!app.panes.focused_mut().tabs[i].trailing_newline);
        app.panes.focused_mut().tabs[i].buf = Buffer::Live(Box::new(
            iced_code_editor::CodeEditor::new("still no newline", "rs"),
        ));
        app.panes.focused_mut().tabs[i].dirty = true;
        app.save_active_tab(false);
        assert_eq!(
            std::fs::read_to_string(dir.join("no-nl.rs")).unwrap(),
            "still no newline",
            "a file without a trailing newline must not gain one"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A save is REFUSED when the file changed on disk under a dirty buffer.
    ///
    /// This is the agent-vs-editor race. Neither version may be discarded silently, so the save
    /// stops and asks — and critically, the file on disk is left exactly as the other writer
    /// left it.
    #[test]
    fn saving_over_a_file_changed_underneath_is_refused() {
        let (mut app, dir) = app_with_file("f.rs", "original\n");
        app.select_file("f.rs".to_string());
        app.panes.focused_mut().tabs[0].buf =
            Buffer::Live(Box::new(iced_code_editor::CodeEditor::new("mine\n", "rs")));
        app.panes.focused_mut().tabs[0].dirty = true;

        // Something else (the agent) writes the file, and the stamp moves.
        std::fs::write(dir.join("f.rs"), "theirs, much longer than the original\n").unwrap();
        app.panes.focused_mut().tabs[0].opened = sc_win::editbuf::DiskStamp {
            mtime: Some(std::time::UNIX_EPOCH),
            len: 9,
        };

        app.save_active_tab(false);

        assert_eq!(
            app.save_conflict.as_deref(),
            Some("f.rs"),
            "refused, and says so"
        );
        assert!(
            app.panes.focused_mut().tabs[0].dirty,
            "the buffer is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("f.rs")).unwrap(),
            "theirs, much longer than the original\n",
            "and THEIR bytes are still on disk"
        );

        // Overwriting is available, but only as an explicit answer.
        app.save_active_tab(true);
        assert_eq!(std::fs::read_to_string(dir.join("f.rs")).unwrap(), "mine\n");
        assert!(app.save_conflict.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// An agent reload must not overwrite a buffer with unsaved edits.
    ///
    /// `live_reload_task` fires every 750ms during a run, so without this guard a run would
    /// erase whatever you were typing in the file it happened to touch.
    #[test]
    fn an_agent_reload_leaves_a_dirty_buffer_alone() {
        let (mut app, dir) = app_with_file("g.rs", "original\n");
        app.select_file("g.rs".to_string());
        app.panes.focused_mut().tabs[0].buf = Buffer::Live(Box::new(
            iced_code_editor::CodeEditor::new("my unsaved work\n", "rs"),
        ));
        app.panes.focused_mut().tabs[0].dirty = true;

        // The agent writes the file, and the app reloads the shown file.
        std::fs::write(dir.join("g.rs"), "agent wrote this\n").unwrap();
        app.reload_selected();

        // `text()` is the widget's `content()`, which rejoins lines without a trailing newline —
        // hence no `\n` here. The save path restores it (see `saving_preserves_the_trailing_newline`).
        assert_eq!(
            app.panes.focused_mut().tabs[0].text().as_deref(),
            Some("my unsaved work"),
            "the dirty buffer survived the reload"
        );
        assert!(app.panes.focused_mut().tabs[0].dirty);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Toggling back restores Assistant, and is exactly one message either way.
    ///
    /// Spec 21 requires the return trip to be as easy as the outbound one — no confirmation, no
    /// extra step. If this ever needs two messages, that requirement has been broken.
    // A craft-only build has one mode, so there is no switch to exercise.
    #[cfg(not(feature = "craft-only"))]
    #[test]
    fn craft_mode_toggles_back_off() {
        let mut app = app_in(Mode::Craft);
        let _ = app.update(Message::ToggleCraftMode(false));
        assert!(!app.cfg.craft());
        assert_eq!(app.cfg.mode, Some(Mode::Assistant), "chosen, not unchosen");
    }

    /// The dialog opens with no model selected.
    ///
    /// The whole reason "None" is the default: a menu click must not silently
    /// spend API credits, and the report is complete without prose.
    #[test]
    fn the_compliance_dialog_defaults_to_no_model() {
        let app = App::default();
        assert_eq!(app.comply_model, ComplyModel::None);
        assert!(!app.comply_open);
        assert!(!app.comply_running);
    }

    /// Opening the dialog clears the previous run's outcome.
    ///
    /// Showing last run's totals beside a fresh dialog invites reading them as
    /// current — on a compliance report that is a real misread.
    #[test]
    fn reopening_the_dialog_drops_the_previous_result() {
        let mut app = App::default();
        app.comply_result = Some(Err("stale".to_string()));
        let _ = app.update(Message::OpenComplyDialog);
        assert!(app.comply_open);
        assert!(app.comply_result.is_none());
    }

    /// A second Run while one is in flight is ignored.
    #[test]
    fn a_second_audit_cannot_be_started_while_one_runs() {
        let mut app = App::default();
        app.comply_running = true;
        app.comply_result = Some(Err("previous".to_string()));
        let _ = app.update(Message::RunComply);
        // Untouched: the guard returned before resetting anything.
        assert!(app.comply_running);
        assert!(app.comply_result.is_some());
    }

    /// A finished audit clears the running flag and records the outcome.
    #[test]
    fn a_failed_audit_reports_its_reason_rather_than_failing_silently() {
        let mut app = App::default();
        app.comply_running = true;
        let _ = app.update(Message::ComplyDone(Err("no workspace".to_string())));
        assert!(!app.comply_running);
        assert!(matches!(app.comply_result, Some(Err(ref e)) if e.contains("no workspace")));
    }

    #[test]
    fn picking_a_model_updates_the_choice() {
        let mut app = App::default();
        let _ = app.update(Message::ComplyModelChanged(ComplyModel::Gemini));
        assert_eq!(app.comply_model, ComplyModel::Gemini);
    }

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
