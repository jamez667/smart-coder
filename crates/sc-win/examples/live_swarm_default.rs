//! Reproduce the GUI's DEFAULT swarm path: only the coder backend is set (orchestrator
//! and advisor fall back to it, exactly as the GUI's settings panel leaves them). This
//! is what a user gets clicking "swarm" without configuring orchestrator/advisor.
//! Run: cargo run -p sc-win --example live_swarm_default

use std::time::{Duration, Instant};

use sc_win::session::{RunKind, Session, UiEvent};
use sc_win::{Topology, UiConfig};

fn main() {
    // Default GUI config: ONLY the coder backend (model + url). No orchestrator/advisor
    // override — they fall back to coder-0, just like the settings panel leaves them.
    let cfg = UiConfig {
        base_url: "http://localhost:11435/v1".to_string(),
        model: "coder-0".to_string(),
        ..UiConfig::default()
    };
    let task = "a websocket powered chat website".to_string();
    let ws = cfg.run_workspace("livecheck");
    println!("workspace: {}", ws.display());
    println!(
        "orchestrator falls back to coder: {} | advisor model: {:?}",
        cfg.orchestrator_model.is_none(),
        cfg.advisor_model
    );
    println!("task: {task}\n");

    let session = Session::spawn(RunKind::Swarm, cfg, task, ws);
    let started = Instant::now();
    let mut topo = Topology::default();
    let deadline = started + Duration::from_secs(240);
    let mut done = false;
    let mut event_count = 0usize;

    while !done && Instant::now() < deadline {
        let now = started.elapsed().as_secs_f32();
        for ev in session.drain_events() {
            event_count += 1;
            match ev {
                UiEvent::Swarm(e) => {
                    if let sc_swarm::SwarmEvent::OrchestratorPrompt {
                        reply, fell_back, ..
                    } = &e
                    {
                        println!("[orchestrator reply, fell_back={fell_back}]:");
                        println!("{}\n", reply.trim());
                    }
                    topo.apply(&e, now);
                    for r in sc_win::view::swarm_rows(&e) {
                        println!("  {}  {}", r.icon, r.text);
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
                UiEvent::Phase { .. } => {}
                UiEvent::Agent(_) => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\n--- {event_count} events seen ---");
    println!("coders: {}", topo.coders().len());
    for c in topo.coders() {
        println!("  {} [{:?}]", c.subtask, c.state);
    }
    if !done {
        println!("=== TIMED OUT (no terminal event in 240s) ===");
    }
}
