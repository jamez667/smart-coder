//! The BUILD step done the DECOMPOSITION way: drive dumb-coder's `staged_build` over the
//! plan-only stage breakdown, one scoped stage at a time, against the real idle-city-sim repo.
//! This tests the harness's ability to break a hard feature into steps a model can each land —
//! NOT a flat whole-feature loop. Any manual fixing here would defeat the point: the code must
//! come from the agent; when it stalls, the fix goes into the HARNESS (dc-workflow/dc-core).
//!   cargo run -p dc-win --example drive_lakes_build

use std::path::PathBuf;

use dc_core::{AgentConfig, AgentEvent};
use dc_model::{ModelBackend, OpenAiBackend};
use dc_workflow::{parse_stages, staged_build};

fn main() {
    let ws = PathBuf::from(r"C:\Users\mail\working\Personal\idle-city-sim");
    let base = std::env::var("DC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("DC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    // Detect the server's real context window (llama.cpp n_ctx) — without this the backend
    // assumes 8192, so the budget is ~6k tokens and a 9k-token file like terrain.rs can NEVER
    // fit the focus-file pin; it gets evicted and the model edits a truncated view.
    let backend = OpenAiBackend::new(base.clone(), model.clone()).with_detected_context();
    println!(
        "backend {base} / {model}  (context {} tok)\nworkspace {}\n",
        backend.capabilities().max_context_tokens,
        ws.display()
    );

    // The ordered stages come from the plan-only breakdown the workflow already produced.
    let breakdown = std::fs::read_to_string(ws.join(".dumb-coder/plan/04-stage-breakdown.md"))
        .expect("run the plan-only workflow first (04-stage-breakdown.md)");
    let stages = parse_stages(&breakdown);
    println!("=== {} STAGES (from the plan-only breakdown) ===", stages.len());
    for (i, s) in stages.iter().enumerate() {
        println!("  {}. {}  [{}]", i + 1, s.title, s.files.join(", "));
    }
    println!();

    let mut cfg = AgentConfig::default();
    cfg.plan_first = false;
    cfg.sandbox = dc_verify::Sandbox::Host;
    cfg.permission.allow_shell = true;

    let sink = dc_core::FnSink(|e: &AgentEvent| match e {
        AgentEvent::ModelTurn { raw, step, .. } => {
            if let Some(n) = dc_win::view::narration(raw) {
                println!("      [{step}] 💭 {n}");
            }
        }
        AgentEvent::ToolCall { tool, arg } => println!("      ▸ {tool} {arg}"),
        // Print edit/tool REJECTIONS (and their first line) so we can see WHY an edit fails to
        // anchor — the key diagnostic for the edit-precision stalls.
        AgentEvent::ToolResult { is_error, summary, full } => {
            if *is_error || full.contains("not found") || full.contains("error") {
                println!("      ⨯ {}", first_line(summary));
                // A bit more of the anchor error to see the old_str mismatch.
                let extra: String = full.lines().take(3).collect::<Vec<_>>().join(" | ");
                println!("        {}", &extra.chars().take(200).collect::<String>());
            }
        }
        AgentEvent::Verification { green, summary, .. } => {
            println!("      {} verify: {summary}", if *green { "✓" } else { "✗" })
        }
        AgentEvent::Stalled { trigger } => println!("      ⚠ stalled: {trigger}"),
        AgentEvent::Diagnosis { report, .. } => println!("      🔬 {}", first_line(report)),
        AgentEvent::Stopped { reason } => println!("      ■ stopped: {reason:?}"),
        _ => {}
    });

    let on_stage = |i: usize, s: &dc_workflow::Stage| {
        println!("\n========== STAGE {} : {} ==========", i + 1, s.title);
    };

    let report = staged_build(
        &backend,
        &stages,
        &ws,
        "cargo check --workspace",
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
            println!("final verified: {}", r.verified);
        }
        Err(e) => println!("ERROR: {e}"),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
