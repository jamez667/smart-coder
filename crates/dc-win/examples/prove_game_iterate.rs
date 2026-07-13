//! Live proof of the ITERATE loop against the real game (idle-city-sim), on a *copy*.
//!
//! Copies the game source (minus target/.git) to a scratch dir, then drives the exact
//! `RunKind::Iterate` path the GUI uses (single agent, host `cargo check`, repo overview)
//! to make one real, small change. Prints the event stream (the same rows the GUI shows),
//! the touched files, and whether `cargo check` ends green — the honest exit check.
//!
//! Never touches the user's real source; the copy is disposable.
//!
//! Run: cargo run -p dc-win --example prove_game_iterate

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dc_win::session::{RunKind, Session, UiEvent};
use dc_win::UiConfig;

const GAME: &str = r"C:\Users\mail\working\Personal\idle-city-sim";
const TASK: &str = "In crates/void_engine/src/app.rs the winit window is created with the title \
     \"City Generator\". Change that window title to \"Idle City Sim\". Change only that \
     title string; do not touch anything else.";

fn main() {
    let src = PathBuf::from(GAME);
    let dst = std::env::temp_dir().join("dc-prove-idle-city-sim");
    let _ = std::fs::remove_dir_all(&dst);
    println!("copying game → {} (excluding target/.git) …", dst.display());
    copy_tree(&src, &dst);
    println!("copied {} files\n", count_files(&dst));

    let cfg = UiConfig {
        base_url: "http://localhost:11439/v1".to_string(),
        model: "qwen3-8b".to_string(),
        // Iterate forces host sandbox + cargo check itself; verify_command left at the
        // default (pytest) is overridden to `cargo check` by the Rust-workspace detection.
        ..UiConfig::default()
    };

    println!("== ITERATE ==\ntask: {TASK}\n");
    let before = read(&dst.join("crates/void_engine/src/app.rs"));
    let session = Session::spawn(RunKind::Iterate, cfg, TASK.to_string(), dst.clone());

    let started = Instant::now();
    let deadline = started + Duration::from_secs(600);
    let mut touched: Vec<String> = Vec::new();
    let mut done = false;
    let mut last_verify: Option<String> = None;
    while !done && Instant::now() < deadline {
        for ev in session.drain_events() {
            if let UiEvent::Agent(e) = &ev {
                if let Some(f) = dc_win::codeview::file_touched_by(e) {
                    if !touched.contains(&f) {
                        touched.push(f);
                    }
                }
                if let dc_core::AgentEvent::Verification { summary, .. } = e {
                    last_verify = Some(summary.clone());
                }
                for r in dc_win::view::agent_rows(e) {
                    println!("  {}  {}", r.icon, r.text);
                }
            }
            match ev {
                UiEvent::Done { ok, summary } => {
                    println!("\n=== {} — {summary}", if ok { "OK" } else { "STOP" });
                    done = true;
                }
                UiEvent::Failed(msg) => {
                    println!("\n=== FAILED: {msg}");
                    done = true;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !done {
        println!("\n=== TIMEOUT after {}s", started.elapsed().as_secs());
    }

    println!("\nfiles the agent touched:");
    for f in &touched {
        println!("  {f}");
    }

    // The concrete proof: did the title actually change, and does it still compile?
    let after = read(&dst.join("crates/void_engine/src/app.rs"));
    let title_changed = before.contains("City Generator") && after.contains("Idle City Sim");
    println!("\ntitle string changed in app.rs: {title_changed}");
    if let Some(v) = &last_verify {
        println!("last verification the agent ran: {v}");
    }

    println!("\n== independent cargo check (host) ==");
    let out = std::process::Command::new("cargo")
        .args(["check", "-p", "city", "--quiet"])
        .current_dir(&dst)
        .output();
    match out {
        Ok(o) => {
            let ok = o.status.success();
            println!("cargo check: {}", if ok { "GREEN ✓" } else { "RED ✗" });
            if !ok {
                let err = String::from_utf8_lossy(&o.stderr);
                for line in err.lines().take(25) {
                    println!("    {line}");
                }
            }
        }
        Err(e) => println!("cargo check could not run: {e}"),
    }
    println!("\n(scratch copy left at {} for inspection)", dst.display());
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// Recursively copy `src` → `dst`, skipping `target` and `.git` (huge / irrelevant).
fn copy_tree(src: &Path, dst: &Path) {
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rel = dir.strip_prefix(src).unwrap();
        let target_dir = dst.join(rel);
        let _ = std::fs::create_dir_all(&target_dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name == "target" || name == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                let _ = std::fs::copy(&p, dst.join(rel).join(name));
            }
        }
    }
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    n += 1;
                }
            }
        }
    }
    n
}
