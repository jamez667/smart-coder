//! Live exit-check harness (not part of the app): drive the SAME `sc_win::Session`
//! the GUI drives, against the live coder-0 backend, and print every streamed
//! `UiEvent`. Proves the real backend → core → event-bridge → honest-stop path the UI
//! renders, without needing to click the window. Run:
//!   cargo run -p sc-win --example live_session

use std::time::{Duration, Instant};

use sc_win::session::{RunKind, Session, UiEvent};
use sc_win::UiConfig;

fn main() {
    let cfg = UiConfig {
        base_url: "http://localhost:11435/v1".to_string(),
        model: "coder-0".to_string(),
        ..UiConfig::default()
    };
    let task = "Create a file hello.txt containing the text: hello from smart-coder".to_string();
    let ws = std::env::temp_dir().join("sc-win-live-check");
    std::fs::create_dir_all(&ws).unwrap();
    println!("workspace: {}", ws.display());
    println!("task: {task}\n");

    let session = Session::spawn(RunKind::Agent, cfg, task, ws.clone());

    // Pump exactly as the GUI does (drain on a tick), with a hard wall-clock cap.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut done = false;
    while !done && Instant::now() < deadline {
        for ev in session.drain_events() {
            match ev {
                UiEvent::Agent(e) => {
                    for r in sc_win::view::agent_rows(&e) {
                        println!("{}  {}", r.icon, r.text);
                    }
                }
                UiEvent::Swarm(e) => {
                    for r in sc_win::view::swarm_rows(&e) {
                        println!("{}  {}", r.icon, r.text);
                    }
                }
                UiEvent::Done { ok, summary } => {
                    println!(
                        "\n=== honest stop: {} {summary}",
                        if ok { "✔" } else { "■" }
                    );
                    done = true;
                }
                UiEvent::Failed(msg) => {
                    println!("\n=== FAILED: {msg}");
                    done = true;
                }
                UiEvent::Phase { .. } => {}
            }
        }
        // The GUI also drains pending confirm/gate requests here; this task needs no
        // shell command, so none should arrive. Auto-deny any that do so we don't hang.
        for p in session.drain_pending() {
            if let sc_win::Pending::Confirm { command, reply, .. } = p {
                println!("(auto-denying unexpected confirm for: {command})");
                let _ = reply.send(sc_core::Confirmation::Deny("live-check".to_string()));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !done {
        println!("\n=== TIMED OUT after 180s");
    }
    let made = ws.join("hello.txt");
    println!("hello.txt exists: {}", made.exists());
    if let Ok(s) = std::fs::read_to_string(&made) {
        println!("hello.txt contents: {s:?}");
    }
}
