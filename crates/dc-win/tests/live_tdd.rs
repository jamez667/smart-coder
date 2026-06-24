//! Live end-to-end proof of the single-agent pivot: plan → write frozen tests →
//! ONE agent loop implements against them in the Docker sandbox until green.
//!
//! This mirrors `session::run_tdd` stage-for-stage but headlessly (no iced loop /
//! no human confirm clicks — AutoApprove + no confirmer), so it can be driven from
//! the terminal against the live Qwen3-8B backend.
//!
//! Requires the live backend (`dc-qwen8b`, :11435) and the Docker sandbox image
//! (`dumb-coder-pyenv`). Ignored by default; run with:
//!   cargo test -p dc-win --test live_tdd -- --ignored --nocapture

use std::sync::atomic::{AtomicUsize, Ordering};

use dc_model::ModelBackend;
use dc_win::config::UiConfig;

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn hello_world_goes_green_with_one_agent() {
    let cfg = UiConfig::default();
    // A fresh, unique workspace each run — real run_tdd stamps with a datetime so a
    // stale source/test file from a prior run never contaminates the gate. Use the
    // process id + a counter env so reruns don't collide.
    let stamp = format!("live-hello-{}", std::process::id());
    let workspace = cfg.run_workspace(&stamp);
    // Start clean even if this pid somehow repeats.
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).expect("create workspace");
    eprintln!("workspace: {}", workspace.display());

    let task = "A hello world website: a single Flask route GET / that returns the \
                text 'Hello, World!' with a 200 status.";

    // --- Stage 1: plan + write frozen tests (same as run_tdd) ---
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let on_phase = |phase: dc_workflow::Phase, _content: &str| {
        eprintln!("[phase] {}", phase.title());
    };
    let outcome = dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        task,
        &workspace,
        dc_workflow::ThinkPolicy::default(),
        &on_phase,
    )
    .expect("workflow ran");

    eprintln!("frozen tests: {:?}", outcome.test_files);
    assert!(
        !outcome.test_files.is_empty(),
        "no frozen tests were written — nothing to implement against"
    );

    // --- Stage 2: ONE agent loop implements against the frozen tests ---
    let verify_cmd = combined_verify_command(&outcome.test_files);
    eprintln!("verify: {verify_cmd}");
    let instruction = format!(
        "Implement this project so ALL the existing tests pass: {task}\n\n\
         The tests are already written and FROZEN — do not edit or delete any test file. \
         Read them to learn the contract, then write the source files. Use run_verification \
         to run the suite; keep editing until green, then finish.\n\nPlan:\n{}",
        outcome
            .state
            .approved()
            .iter()
            .map(|a| format!("=== {} ===\n{}", a.phase.title(), a.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    );

    let backend = cfg.backend();
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some(verify_cmd);
    agent_cfg.permission.frozen_paths = outcome.test_files.clone();
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;

    let steps = AtomicUsize::new(0);
    let sink = dc_core::FnSink(|e: &dc_core::AgentEvent| {
        let n = steps.fetch_add(0, Ordering::Relaxed);
        eprintln!("[agent#{n}] {e:?}");
    });

    let report = dc_core::run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        &workspace,
        &agent_cfg,
        &sink,
    )
    .expect("agent ran");

    eprintln!(
        "RESULT: finished={} verified={:?} steps={}",
        report.finished, report.verified, report.steps
    );
    assert_eq!(
        report.verified,
        Some(true),
        "tests did not go green (steps={})",
        report.steps
    );
}

/// Local copy of `session::combined_verify_command` (that fn is private to the bin).
fn combined_verify_command(test_files: &[String]) -> String {
    let has_py = test_files.iter().any(|f| f.ends_with(".py"));
    let has_js = test_files.iter().any(|f| f.ends_with(".test.js"));
    let mut parts = Vec::new();
    if has_py {
        parts.push("python -m pytest -q".to_string());
    }
    if has_js {
        parts.push("vitest run".to_string());
    }
    if parts.is_empty() {
        "python -m pytest -q".to_string()
    } else {
        parts.join(" && ")
    }
}
