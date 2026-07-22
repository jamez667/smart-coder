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
// `impl App` is split across these submodules (each adds its own impl block):
mod logic_a;
mod logic_b;
mod logic_c;
mod update;
mod view_code;
mod view_core;
mod view_menus;
mod view_panels;

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
