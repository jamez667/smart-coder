//! The BUILD step after the plan-only design: drive the real iterate agent loop against
//! idle-city-sim to actually implement lakes, grounded in the plan + the design artifacts the
//! plan-only workflow produced, verifying with `cargo check --workspace` until green.
//!
//! This is what a future "Build" button would run. Headless so I can watch it iterate.
//!   cargo run -p dc-win --example drive_lakes_build

use std::path::PathBuf;

use dc_core::{run_agent_observed, select_strategy, AgentConfig, AgentEvent};
use dc_model::{ModelBackend, OpenAiBackend};

fn main() {
    let ws = PathBuf::from(r"C:\Users\mail\working\Personal\idle-city-sim");
    let base = std::env::var("DC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("DC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let backend = OpenAiBackend::new(base.clone(), model.clone());
    println!("backend {base} / {model}\nworkspace {}\n", ws.display());

    // Ground the build in the plan + the design artifacts the plan-only run produced.
    let plan = read(&ws, "PLAN-lakes.md");
    let specs = read_plan_artifact(&ws, "01-specs.md");
    let arch = read_plan_artifact(&ws, "02-architecture.md");
    let layout = read_plan_artifact(&ws, "03-layout.md");
    let breakdown = read_plan_artifact(&ws, "04-stage-breakdown.md");

    let instruction = format!(
        "You are editing the EXISTING Rust project `idle-city-sim` in place to FINISH implementing \
         LAKES in the terrain. Progress so far (already on disk, compiles): a `Lake` struct, a \
         `lakes: Vec<Lake>` field on `Terrain`, and a `generate_lakes()` method that populates \
         it (called from `Terrain::generate`). Each `Lake` has `center: Vec2`, `radius: f32`, \
         `elevation: f32`, `shape: Vec2`.\n\n\
         WHAT REMAINS — the lakes are generated but INVISIBLE because nothing reads `self.lakes`. \
         Wire them in (surgical edit_file only):\n\
         1. Add an `on_lake(&self, pt: Vec2) -> bool` helper: true if `pt` is within a lake's \
         radius of its center (loop over `self.lakes`).\n\
         2. In `water_at`, before the ocean check, return Some(WaterKind::Lake) when \
         self.on_lake(pt) is true.\n\
         3. In `water_depth`, return `Some((WaterKind::Lake, <depth>))` for a lake point (a \
         moderate depth like 0.5 is fine).\n\
         4. In `render_sample`, a lake point must return a water tile so `draw_land` colors it. \
         Read how Ocean is returned there and mirror it for lakes (the `RenderTile` enum — do NOT \
         change its shape).\n\
         Read the file first, edit surgically, and run_verification until it PASSES, then finish. \
         Do NOT rewrite whole files or touch unrelated code.\n\n\
         Original context below for reference.\n\
         Editing the EXISTING Rust project `idle-city-sim` in place to implement LAKES in the \
         terrain.\n\n\
         CRITICAL — HOW TO EDIT: terrain.rs is ~730 lines of working code the rest of the crate \
         depends on. Make SURGICAL additions with edit_file — add a `Lake` struct, add a `lakes` \
         field to the existing `Terrain` struct, add new methods, and add match arms to the \
         EXISTING `water_at`/`water_depth`/`render_sample`. Do NOT use write_file to rewrite the \
         whole file — you will drop existing functions (elevation math, the river network, \
         RenderTile is an ENUM with Ocean/Land variants) and nothing will compile. Change only \
         what lakes need; leave everything else exactly as it is.\n\n\
         IMPORTANT — the verify command checks that lakes are ACTUALLY implemented, not just \
         that the code compiles. It will FAIL until you have: (1) a `Lake` struct or a `lakes` \
         field holding lake data on `Terrain`, (2) a function that GENERATES lakes (called from \
         `Terrain::generate`), and (3) `WaterKind::Lake` returned from the water classifiers so \
         lakes actually render. Normalizing whitespace or making a no-op edit will NOT pass — you \
         must write real lake logic. When run_verification fails it now shows the compiler \
         errors — read them and fix exactly those. Do not call finish until run_verification \
         passes.\n\n\
         The feature plan:\n{plan}\n\n\
         The approved design (produced by the planning pass):\n\
         === SPECS ===\n{specs}\n\n=== ARCHITECTURE ===\n{arch}\n\n=== LAYOUT ===\n{layout}\n\n\
         === STAGE BREAKDOWN (build in this order) ===\n{breakdown}\n\n\
         Implementation notes about THIS codebase (verified):\n\
         - Elevation is a continuous function on `Terrain` (crates/city/src/gen/terrain.rs), \
         NOT a grid. `elevation_bare(pt)` is the raw land field; `elevation(pt)` adds river \
         carving.\n\
         - `WaterKind::Lake` ALREADY EXISTS (terrain.rs) and is already colored in palette.rs \
         (`water_depth_color(WaterKind::Lake, depth)`), so the plumbing is present.\n\
         - Route lakes through the water classifiers: `water_at(pt)`, `water_depth(pt)`, \
         `is_water_render(pt)`, and the per-tile `render_sample(pt, want_shade)` which returns a \
         `RenderTile` enum. Add a lake branch/variant there.\n\
         - The renderer (crates/city/src/render.rs) fills water in `draw_land` from \
         `render_sample`; add a lake arm calling `palette::water_depth_color(WaterKind::Lake, \
         depth)`. Contours and City.water handle Lake automatically once `water_at` returns it.\n\
         - Model a `Lake` on the existing `River` struct if you add explicit primitives; build \
         them in `Terrain::generate`. Reuse `noise::fbm` for shapes and `rng::Pcg32` for \
         placement.\n\
         Start small: get lakes into `water_at`/`render_sample` so they render, keep \
         `cargo check` green at every step, then add river-connection and elevation logic."
    );

    let mut cfg = AgentConfig {
        // A CONTRACT verify: fails until lakes are genuinely wired (Lake data + a generator +
        // WaterKind::Lake routed through), THEN runs cargo check. Stops the model declaring
        // false victory with a no-op edit that happens to compile (observed: it stripped CRLFs,
        // changed nothing, and "passed" a bare cargo check).
        verify_command: Some(
            "pwsh -NoProfile -File .dc/verify_lakes.ps1".to_string(),
        ),
        plan_first: false,
        sandbox: dc_verify::Sandbox::Host,
        ..AgentConfig::default()
    };
    cfg.max_steps = 80;
    // Allow shell so cargo runs; no frozen paths (edit anything).
    cfg.permission.allow_shell = true;

    let sink = dc_core::FnSink(|e: &AgentEvent| match e {
        AgentEvent::ModelTurn { raw, step, .. } => {
            if let Some(n) = dc_win::view::narration(raw) {
                println!("[{step}] 💭 {n}");
            }
        }
        AgentEvent::ToolCall { tool, arg } => println!("      ▸ {tool} {arg}"),
        AgentEvent::Verification { green, summary, .. } => {
            println!("      {} verify: {summary}", if *green { "✓" } else { "✗" })
        }
        AgentEvent::Stalled { trigger } => println!("      ⚠ stalled: {trigger}"),
        AgentEvent::Diagnosis { report, .. } => println!("      🔬 {}", first_line(report)),
        AgentEvent::Stopped { reason } => println!("      ■ stopped: {reason:?}"),
        _ => {}
    });

    let registry = dc_tools::default_registry();
    let strategy = select_strategy(&backend.capabilities());
    println!("=== BUILDING (cargo check --workspace as the gate) ===\n");
    let report = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        &ws,
        &cfg,
        &sink,
    );

    println!("\n=== RESULT ===");
    match report {
        Ok(r) => {
            println!("finished: {}   verified: {:?}   steps: {}", r.finished, r.verified, r.steps);
            println!("stop: {:?}", r.stop_reason);
            println!("change summary: {}", r.change_summary);
        }
        Err(e) => println!("ERROR: {e}"),
    }
}

fn read(ws: &std::path::Path, rel: &str) -> String {
    std::fs::read_to_string(ws.join(rel)).unwrap_or_else(|_| format!("(missing {rel})"))
}
fn read_plan_artifact(ws: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(ws.join(".dumb-coder").join("plan").join(name))
        .unwrap_or_else(|_| format!("(missing {name})"))
}
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
