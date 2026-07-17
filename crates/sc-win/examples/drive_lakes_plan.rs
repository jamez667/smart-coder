//! Reproduce EXACTLY what the UI does when you click "⚒ Execute plan" on the real
//! idle-city-sim/PLAN-lakes.md, step by step, verifying each output. Same code path as the
//! GUI: is_feature_plan gate → plan_task() → ProjectStack::detect → run_workflow_moded(plan_only)
//! with the same on_phase streaming the Plan panel uses.
//!
//! Run with the 30B up:  cargo run -p sc-win --example drive_lakes_plan

use std::path::PathBuf;

use sc_model::OpenAiBackend;
use sc_workflow::{run_workflow_moded, AutoApprove, Phase, ProjectStack, ThinkPolicy, WorkflowMode};

// Mirror of app.rs::is_feature_plan (private there) — the button's gate.
fn is_feature_plan(name: &str) -> bool {
    let n = name.trim();
    n.to_ascii_uppercase().starts_with("PLAN-") && n.to_ascii_lowercase().ends_with(".md")
}

// Mirror of app.rs::plan_task (private there) — the exact task the UI builds.
fn plan_task(plan_name: &str) -> String {
    format!(
        "Design how to implement the feature plan in {plan_name}. Read the plan, look at the \
         relevant existing files, and produce a spec, an architecture, a file layout, and an \
         ordered implementation breakdown that follows the plan's Approach and Files-to-touch. \
         This is a DESIGN pass — do not write source code yet."
    )
}

fn check(label: &str, ok: bool) {
    println!("  [{}] {label}", if ok { "PASS" } else { "FAIL" });
}

fn main() {
    let ws = PathBuf::from(r"C:\Users\mail\working\Personal\idle-city-sim");
    let plan_name = "PLAN-lakes.md";
    let base = std::env::var("SC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("SC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let backend = OpenAiBackend::new(base.clone(), model.clone());

    println!("=== STEP 0: preconditions the UI checks ===");
    check("workspace exists (picked_workspace)", ws.is_dir());
    check("plan file exists on disk", ws.join(plan_name).is_file());
    check("is_feature_plan gate (button shows)", is_feature_plan(plan_name));
    println!("  backend: {base}  model: {model}");

    println!("\n=== STEP 1: ProjectStack::detect (language-aware) ===");
    let stack = ProjectStack::detect(&ws);
    println!("  detected: {}", stack.label());
    check("detected Rust (Cargo.toml present)", stack == ProjectStack::Rust);

    println!("\n=== STEP 2: the task the UI builds (plan_task) ===");
    let task = plan_task(plan_name);
    println!("  {task}");
    check("names the plan (so referenced_plan pins it)", task.contains(plan_name));

    println!("\n=== STEP 3: run the plan-only workflow (run_plan → run_workflow_moded) ===");
    println!("  (each phase below is what streams to the Plan panel)\n");
    let seen = std::cell::RefCell::new(Vec::new());
    let on_phase = |p: Phase, content: &str| {
        seen.borrow_mut().push(p);
        println!("──────── PHASE: {} ────────", p.title());
        println!("{}\n", content.trim());
    };

    let outcome = match run_workflow_moded(
        &backend,
        &backend,
        &task,
        &ws,
        ThinkPolicy::default(),
        WorkflowMode::plan_only(),
        &on_phase,
        &AutoApprove,
    ) {
        Ok(o) => o,
        Err(e) => {
            println!("  WORKFLOW ERROR: {e}");
            std::process::exit(1);
        }
    };

    println!("\n=== STEP 4: verify each output ===");
    let phases = seen.into_inner();

    // 4a. The exact four design phases ran, in order, then stopped.
    let expected = vec![Phase::Specs, Phase::Architecture, Phase::Layout, Phase::StageBreakdown];
    check("ran specs→architecture→layout→breakdown then STOPPED", phases == expected);

    // 4b. Plan-only guarantees: no tests, no build.
    check("no frozen tests written", outcome.test_files.is_empty());
    check("no decomposition/build board", outcome.board.len() == 0);
    check("not aborted", !outcome.aborted);

    // 4c. Each artifact is non-empty and persisted to .smart-coder/plan/ (what the panel reads).
    for p in &expected {
        let art = outcome.state.artifact(*p);
        let non_empty = art.map(|a| a.content.trim().len() > 40).unwrap_or(false);
        check(&format!("{} artifact is substantive", p.title()), non_empty);
        let file = sc_workflow::plan_dir(&ws).join(p.filename());
        check(&format!("{} persisted to disk", p.filename()), file.is_file());
    }

    // 4d. Language fidelity — the content must be Rust, never Flask/Python.
    let all: String = outcome
        .state
        .approved()
        .iter()
        .map(|a| a.content.to_lowercase())
        .collect();
    check("mentions rust/cargo/.rs somewhere", all.contains("rust") || all.contains("cargo") || all.contains(".rs"));
    check("NEVER mentions flask", !all.contains("flask"));
    check("NEVER mentions app.py", !all.contains("app.py"));
    check("NEVER mentions pytest", !all.contains("pytest"));

    // 4e. Plan fidelity — did it actually engage with THIS plan (lakes/terrain, the named files)?
    check("engages the plan's subject (lake/terrain/water)",
        all.contains("lake") || all.contains("terrain") || all.contains("water"));
    check("references a plan-named file (terrain.rs or render.rs)",
        all.contains("terrain.rs") || all.contains("render.rs"));

    // 4f. The breakdown is an ORDERED design (not a pytest coverage JSON array).
    let breakdown = outcome
        .state
        .artifact(Phase::StageBreakdown)
        .map(|a| a.content.clone())
        .unwrap_or_default();
    check("breakdown is prose/markdown, not a JSON coverage array",
        !breakdown.trim_start().starts_with('[') && !breakdown.contains("\"covers\""));

    println!("\n=== DONE ===");
    println!("Artifacts on disk under: {}", sc_workflow::plan_dir(&ws).display());
}
