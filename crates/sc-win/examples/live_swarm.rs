//! Live check for the swarm topology: drive the SAME `sc_win::Session` (swarm mode)
//! the GUI drives, against the live backends, folding the stream into a `Topology`
//! and printing the node/flow state — proving the canvas's data model populates from
//! real orchestrator/coder/advisor events. Run:
//!   cargo run -p sc-win --example live_swarm

use std::time::{Duration, Instant};

use sc_win::session::{RunKind, Session, UiEvent};
use sc_win::{Topology, UiConfig};

fn main() {
    let cfg = UiConfig {
        base_url: "http://localhost:11435/v1".to_string(),
        model: "coder-0".to_string(),
        // Orchestrator (decomposer) and advisor on the e4b box.
        orchestrator_url: Some("http://localhost:11434/v1".to_string()),
        orchestrator_model: Some("advisor-e4b".to_string()),
        advisor_url: Some("http://localhost:11434/v1".to_string()),
        advisor_model: Some("advisor-e4b".to_string()),
        max_workers: 2,
        ..UiConfig::default()
    };
    let task = "Create two files: login.html (a basic login form) and hello.html \
                (a hello world page)."
        .to_string();
    // Use the SAME isolated scratch workspace the GUI now defaults to — proves a swarm
    // writes there, never into the launch/source dir.
    let ws = cfg.run_workspace("livecheck");
    println!("workspace: {}", ws.display());
    println!("task: {task}\n");

    let session = Session::spawn(RunKind::Swarm, cfg, task, ws.clone());
    let started = Instant::now();
    let mut topo = Topology::default();

    let deadline = started + Duration::from_secs(240);
    let mut done = false;
    while !done && Instant::now() < deadline {
        let now = started.elapsed().as_secs_f32();
        for ev in session.drain_events() {
            match ev {
                UiEvent::Swarm(e) => {
                    // Show the decomposition prompt+reply when it arrives.
                    if let sc_swarm::SwarmEvent::OrchestratorPrompt {
                        reply, fell_back, ..
                    } = &e
                    {
                        println!("\n=== ORCHESTRATOR reply (fell_back={fell_back}) ===");
                        println!("{}", reply.trim());
                        println!("=== end reply ===\n");
                    }
                    topo.apply(&e, now);
                    for r in sc_win::view::swarm_rows(&e) {
                        println!("{}  {}", r.icon, r.text);
                    }
                }
                UiEvent::Done { ok, summary } => {
                    println!("\n=== {} {summary}", if ok { "✔" } else { "■" });
                    done = true;
                }
                UiEvent::Failed(msg) => {
                    println!("\n=== FAILED: {msg}");
                    done = true;
                }
                UiEvent::Phase { .. } => {}
                UiEvent::Agent(_) => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Report the final topology the canvas would draw.
    println!("\n--- topology ---");
    println!(
        "orchestrator: {} | advisor used: {} | done: {}",
        if topo.decomposed { "active" } else { "idle" },
        topo.advisor_used,
        topo.done
    );
    for c in topo.coders() {
        let advice = c
            .last_advice
            .as_deref()
            .map(|a| format!("  (advice: {a})"))
            .unwrap_or_default();
        println!("  coder {} [{:?}] — {}{advice}", c.subtask, c.state, c.goal);
    }
}
