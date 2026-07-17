//! Live check for ITERATION: build a small app in a folder, then run a SECOND prompt
//! in the SAME folder ("add X") and confirm the decomposer sees the existing files
//! (repo overview) and the workers edit them rather than starting blank.
//! Run: cargo run -p sc-win --example live_iterate

use std::time::{Duration, Instant};

use sc_win::session::{RunKind, Session, UiEvent};
use sc_win::{Topology, UiConfig};

fn run(cfg: &UiConfig, task: &str, ws: &std::path::Path, label: &str) {
    println!("\n############## {label} ##############");
    println!("task: {task}");
    let overview = sc_win::config::repo_overview(ws);
    if overview.is_empty() {
        println!("(workspace empty — from scratch)\n");
    } else {
        println!("repo overview fed to decomposer:\n{overview}");
    }
    let session = Session::spawn(
        RunKind::Swarm,
        cfg.clone(),
        task.to_string(),
        ws.to_path_buf(),
    );
    let started = Instant::now();
    let mut topo = Topology::default();
    let deadline = started + Duration::from_secs(240);
    let mut done = false;
    while !done && Instant::now() < deadline {
        let now = started.elapsed().as_secs_f32();
        for ev in session.drain_events() {
            match ev {
                UiEvent::Swarm(e) => {
                    if let sc_swarm::SwarmEvent::OrchestratorPrompt { reply, .. } = &e {
                        println!("[decomposition]:\n{}\n", reply.trim());
                    }
                    topo.apply(&e, now);
                    for r in sc_win::view::swarm_rows(&e) {
                        println!("  {}  {}", r.icon, r.text);
                    }
                }
                UiEvent::Done { ok, summary } => {
                    println!("=== {} {summary}", if ok { "OK" } else { "STOP" });
                    done = true;
                }
                UiEvent::Failed(msg) => {
                    println!("=== FAILED: {msg}");
                    done = true;
                }
                UiEvent::Phase { .. } => {}
                UiEvent::Agent(_) => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn main() {
    let cfg = UiConfig {
        base_url: "http://localhost:11435/v1".to_string(),
        model: "coder-0".to_string(),
        ..UiConfig::default()
    };
    // One fixed folder, used for BOTH prompts — the "pick a folder and iterate" path.
    let ws = cfg.workspace.join("iterate-demo");
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    println!("workspace: {}", ws.display());

    // 1) Build it.
    run(&cfg, "a simple flask hello-world web app", &ws, "BUILD");
    println!("\nfiles now present:");
    for f in walk(&ws) {
        println!("  {f}");
    }

    // 2) Iterate on it — same folder. The decomposer should see the existing files.
    run(
        &cfg,
        "add a /time route that shows the current server time",
        &ws,
        "ITERATE",
    );
    println!("\nfiles after iteration:");
    for f in walk(&ws) {
        println!("  {f}");
    }
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(
                        p.strip_prefix(dir)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
    }
    out.sort();
    out
}
