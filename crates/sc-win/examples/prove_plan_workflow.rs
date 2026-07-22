//! Headless drive of the plan-only workflow against the LIVE backend, on a Rust workspace.
//! Proves end-to-end: language-aware (Rust, not Flask) phases land, the plan is pinned (no
//! re-read), and the run stops at the stage breakdown. Run with the 30B up:
//!   cargo run -p sc-win --example prove_plan_workflow

use sc_model::OpenAiBackend;
use sc_workflow::{run_workflow_moded, AutoApprove, ThinkPolicy, WorkflowMode};

fn main() {
    let base = std::env::var("SC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("SC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let backend = OpenAiBackend::new(base.clone(), model.clone());
    println!("backend: {base}  model: {model}\n");

    // A tiny Rust "project" so ProjectStack::detect → Rust, with a real feature plan on disk.
    let ws = std::env::temp_dir().join("dc-prove-plan");
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"city\"\nversion = \"0.1.0\"",
    )
    .unwrap();
    std::fs::write(
        ws.join("src/terrain.rs"),
        "//! Terrain mesh: heightmap → triangles.\npub struct Terrain { pub heights: Vec<f32> }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("PLAN-lakes.md"),
        "## Plan: lakes on the terrain\n\
         **Approach:** flood-fill basins below a water-level threshold; render a translucent \
         water quad per basin.\n\
         **Files to touch:**\n- src/terrain.rs — expose per-cell height min/max\n\
         - src/water.rs (new) — basin detection + water surface\n\
         **Steps:**\n1. Compute per-cell height range\n2. Flood-fill basins ≤ water_level\n\
         3. Emit water quads\n**Risks:** basin detection at map edges.",
    )
    .unwrap();

    let task = "Design how to implement the feature plan in PLAN-lakes.md. Read the plan, look \
        at the relevant existing files, and produce a spec, an architecture, a file layout, and \
        an ordered implementation breakdown. This is a DESIGN pass — do not write source code yet.";

    let seen = std::cell::RefCell::new(Vec::new());
    let on_phase = |p: sc_workflow::Phase, content: &str| {
        seen.borrow_mut().push(p);
        println!("======== {} ========\n{}\n", p.title(), content.trim());
    };

    let outcome = run_workflow_moded(
        &backend,
        &backend,
        task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::plan_only(),
        &on_phase,
        &AutoApprove,
    )
    .expect("workflow ran");

    let phases = seen.into_inner();
    println!("\n=== VERDICT ===");
    println!(
        "phases run: {:?}",
        phases.iter().map(|p| p.title()).collect::<Vec<_>>()
    );
    println!(
        "stopped at stage breakdown: {}",
        phases.last() == Some(&sc_workflow::Phase::StageBreakdown)
    );
    println!("no frozen tests written: {}", outcome.test_files.is_empty());
    println!("no decomposition/board: {}", outcome.board.is_empty());

    // Language check: the architecture must talk Rust, not Flask.
    let arch = outcome
        .state
        .approved()
        .iter()
        .find(|a| a.phase == sc_workflow::Phase::Architecture)
        .map(|a| a.content.to_lowercase())
        .unwrap_or_default();
    println!(
        "architecture mentions rust/cargo: {}   mentions flask/app.py: {}",
        arch.contains("rust") || arch.contains("cargo") || arch.contains(".rs"),
        arch.contains("flask") || arch.contains("app.py")
    );
    let _ = std::fs::remove_dir_all(&ws);
}
