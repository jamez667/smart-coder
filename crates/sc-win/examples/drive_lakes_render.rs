//! FOLLOW-UP drive: wire lake RENDERING. The main lakes build got the data model right (water_at
//! returns Lake, behavioral oracle green) but the renderer colors tiles from `render_sample`, which
//! never learned about lakes — so lakes are invisible. This drives the render integration as its
//! own scoped staged-build, gated on a SECOND frozen oracle (`lakes_render_oracle`) that fails
//! unless render_sample classifies lake points as water. As always: the code comes from the agent;
//! stalls get fixed in the HARNESS, never here.
//!   cargo run -p sc-win --example drive_lakes_render

use std::path::PathBuf;

use sc_core::{AgentConfig, AgentEvent};
use sc_model::{ModelBackend, OpenAiBackend};
use sc_workflow::{staged_build, Stage};

fn main() {
    let ws = PathBuf::from(r"C:\Users\mail\working\Personal\idle-city-sim");
    let base = std::env::var("SC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("SC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let backend = OpenAiBackend::new(base.clone(), model.clone()).with_detected_context();
    println!(
        "backend {base} / {model}  (context {} tok)\nworkspace {}\n",
        backend.capabilities().max_context_tokens,
        ws.display()
    );

    // ONE render stage, gated on the render ORACLE (not just cargo check). The two-stage split
    // failed because stage 1 could COMPILE (enum variant + a draw_land arm) without render_sample
    // ever RETURNING the lake variant — the compile gate passed on a half-done change. The render
    // path spans three tightly-coupled edits (enum, render_sample classify, draw_land color) that
    // must all land together to satisfy the behavioral gate, so they belong in one oracle-gated
    // stage with all three files pinned — not artificially sliced by file.
    let stages = vec![Stage {
        title: "Route lakes through the render path".into(),
        files: vec![
            "crates/city/src/gen/terrain/mod.rs".into(),
            "crates/city/src/gen/terrain/water.rs".into(),
            "crates/city/src/render.rs".into(),
        ],
        instruction: "Lakes exist in the data model (water_at returns WaterKind::Lake) but never \
            DRAW, because the renderer colors tiles from `Terrain::render_sample` (in water.rs), \
            which only knows Ocean and Land. Make lakes render as blue water with THREE coupled \
            edits: (1) in mod.rs add a `Lake(f32)` variant to the `RenderTile` enum (f32 = depth \
            [0,1], like Ocean). (2) in water.rs `render_sample`, after the below-sea Ocean check \
            and BEFORE `RenderTile::Land {..}`, test whether `pt` is inside a lake — iterate \
            `self.lakes`, inside when `(pt - lake.center).length() < lake.radius` — and if so \
            `return RenderTile::Lake(lake.depth);`. (3) in render.rs `draw_land`, add a match arm \
            `RenderTile::Lake(depth)` that colors the tile with \
            `palette::water_depth_color(crate::gen::terrain::WaterKind::Lake, depth)`, like the \
            Ocean arm but with WaterKind::Lake. All three must be present for lakes to draw. Do NOT \
            edit the oracle files or main.rs.\n\n\
            CRITICAL: make these edits ONE AT A TIME. Each reply must be exactly ONE JSON tool call \
            and nothing else — no prose, no plan, no ```json fence, no multiple edits batched. Do \
            edit (1) FIRST: reply with a single edit_file/edit_lines call that adds the enum \
            variant, and STOP. Wait for the result, then do (2), then (3). Do not describe what you \
            will do — just emit the one tool call."
            .into(),
    }];

    println!("=== {} RENDER STAGE ===", stages.len());
    for (i, s) in stages.iter().enumerate() {
        println!("  {}. {}  [{}]", i + 1, s.title, s.files.join(", "));
    }
    println!();

    let mut cfg = AgentConfig {
        plan_first: false,
        sandbox: sc_verify::Sandbox::Host,
        ..Default::default()
    };
    cfg.permission.allow_shell = true;
    // Freeze BOTH oracles and main.rs so the model can't game either gate.
    cfg.permission.frozen_paths = vec![
        "crates/city/src/lakes_oracle.rs".to_string(),
        "crates/city/src/lakes_render_oracle.rs".to_string(),
        "crates/city/src/main.rs".to_string(),
    ];

    let sink = sc_core::FnSink(|e: &AgentEvent| match e {
        AgentEvent::ModelTurn { raw, step, .. } => {
            if let Some(n) = sc_win::view::narration(raw) {
                println!("      [{step}] 💭 {n}");
            }
        }
        AgentEvent::ToolCall { tool, arg } => println!("      ▸ {tool} {arg}"),
        AgentEvent::ToolResult {
            is_error,
            summary,
            full,
        } if (*is_error || full.contains("not found") || full.contains("error")) => {
            println!("      ⨯ {}", first_line(summary));
            let extra: String = full.lines().take(3).collect::<Vec<_>>().join(" | ");
            println!("        {}", extra.chars().take(200).collect::<String>());
        }
        AgentEvent::Verification { green, summary, .. } => {
            println!("      {} verify: {summary}", if *green { "✓" } else { "✗" })
        }
        AgentEvent::Stalled { trigger } => println!("      ⚠ stalled: {trigger}"),
        AgentEvent::Diagnosis { report, .. } => println!("      🔬 {}", first_line(report)),
        AgentEvent::Stopped { reason } => println!("      ■ stopped: {reason:?}"),
        _ => {}
    });

    let on_stage = |i: usize, s: &sc_workflow::Stage| {
        println!("\n========== STAGE {} : {} ==========", i + 1, s.title);
    };

    // Gate each stage on cargo check; the FINAL oracle is the RENDER behavioral test.
    let report = staged_build(
        &backend,
        &stages,
        &ws,
        "cargo check --workspace",
        Some("cargo test -p city lakes_render_oracle"),
        &cfg,
        &on_stage,
        &sink,
    );

    println!("\n=== RESULT ===");
    match report {
        Ok(r) => {
            for st in &r.stages {
                println!(
                    "  [{}]{} {} ({} steps)",
                    if st.verified { "green" } else { "RED  " },
                    if st.changed { "" } else { " NO-OP" },
                    st.title,
                    st.steps
                );
            }
            match r.oracle_passed {
                Some(true) => println!("\n🌊 RENDER ORACLE PASSED — lakes are drawn as water."),
                Some(false) => println!(
                    "\n❌ RENDER ORACLE FAILED — lakes still not routed through render_sample."
                ),
                None => println!("\n(no render oracle configured)"),
            }
            println!("final verified: {}", r.verified);
        }
        Err(e) => println!("ERROR: {e}"),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
