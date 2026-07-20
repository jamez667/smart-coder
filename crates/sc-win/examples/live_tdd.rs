//! Live check for the TDD workflow: drive the SAME `sc_win::Session` (Tdd mode) the
//! "⚒ build (TDD)" button uses, and show the plan phases streaming + the frozen tests
//! written first. Run: cargo run -p sc-win --example live_tdd

use std::time::{Duration, Instant};

use sc_win::session::{RunKind, Session, UiEvent};
use sc_win::{Plan, UiConfig};

fn main() {
    let cfg = UiConfig {
        base_url: "http://localhost:11435/v1".to_string(),
        model: "coder-0".to_string(),
        ..UiConfig::default()
    };
    let task = "hello world website".to_string();
    let ws = cfg.run_workspace("tdd-livecheck");
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    println!("workspace: {}", ws.display());
    println!("verify: {:?}", cfg.verify_command);
    println!("task: {task}\n");

    let session = Session::spawn(RunKind::Tdd, cfg, task, ws.clone());
    let mut plan = Plan::default();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(300);
    let mut done = false;
    while !done && Instant::now() < deadline {
        for ev in session.drain_events() {
            match ev {
                UiEvent::Phase {
                    phase,
                    content,
                    tests_written,
                    dir,
                } => {
                    plan.apply(phase, &content, &tests_written, dir.as_deref());
                    if tests_written.is_empty() {
                        println!("◆ PHASE {}", phase.title());
                        for l in content.lines().take(4) {
                            println!("    {l}");
                        }
                    } else {
                        println!("✓ FROZEN TESTS WRITTEN: {}", tests_written.join(", "));
                    }
                }
                UiEvent::Swarm(e) => {
                    // Print the raw reject reason for diagnosis (swarm_rows hides it).
                    if let sc_swarm::SwarmEvent::Integrated {
                        subtask,
                        accepted: false,
                        files,
                    } = &e
                    {
                        println!("  ✗  [{subtask}] REVERTED — {}", files.join(", "));
                    } else {
                        for r in sc_win::view::swarm_rows(&e) {
                            println!("  {}  {}", r.icon, r.text);
                        }
                    }
                }
                UiEvent::Done { ok, summary } => {
                    println!("\n=== {} {summary}", if ok { "OK" } else { "STOP" });
                    done = true;
                }
                UiEvent::Failed(msg) => {
                    println!("\n=== FAILED: {msg}");
                    done = true;
                }
                UiEvent::Agent(_) => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\n--- plan summary ---");
    println!(
        "phases done: {}",
        plan.steps().iter().filter(|s| s.done).count()
    );
    println!("frozen tests: {:?}", plan.frozen_tests);
    println!("subtasks: {:?}", plan.subtasks);
    println!("\n--- files on disk ---");
    let mut stack = vec![ws.clone()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    println!("  {}", p.strip_prefix(&ws).unwrap_or(&p).display());
                }
            }
        }
    }
}
