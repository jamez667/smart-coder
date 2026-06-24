//! A/B: the SAME prompt through (A) the full dumb-coder pipeline vs (B) one raw
//! completion to the same model — judged by the SAME frozen tests, run in the same
//! Docker sandbox. Isolates the harness's value (plan → frozen tests → agent loop →
//! verify) from the raw model.
//!
//! Requires the live backend (`dc-qwen8b`, :11435) + the sandbox image
//! (`dumb-coder-pyenv`). Ignored by default; run with:
//!   cargo test -p dc-win --test ab_compare -- --ignored --nocapture

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_win::config::UiConfig;

/// The shared task. A few routes + a behavior so the harness's value can show — more
/// surface than hello-world, still small enough to land in one session.
const TASK: &str = "A tiny Flask JSON API with three routes: \
    GET /health returns {\"status\":\"ok\"} with 200; \
    GET /greet?name=X returns {\"greeting\":\"Hello, X!\"} with 200 (default name 'World'); \
    POST /sum with JSON {\"a\":int,\"b\":int} returns {\"sum\":a+b} with 200. \
    The app object must be importable as `from app import app`.";

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn ab_pipeline_vs_raw_model() {
    let cfg = UiConfig::default();

    // ===== ARM A: full dumb-coder pipeline =====
    let ws_a = fresh_ws(&cfg, "ab-pipeline");
    eprintln!(
        "\n===== ARM A (dumb-coder pipeline) =====\nworkspace: {}",
        ws_a.display()
    );

    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let outcome = dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        TASK,
        &ws_a,
        dc_workflow::ThinkPolicy::default(),
        &|p: dc_workflow::Phase, _c: &str| eprintln!("[A phase] {}", p.title()),
    )
    .expect("workflow ran");
    eprintln!("[A] frozen tests: {:?}", outcome.test_files);
    assert!(
        !outcome.test_files.is_empty(),
        "Arm A wrote no frozen tests"
    );

    let verify_cmd = scoped_pytest(&outcome.test_files);
    eprintln!("[A] verify: {verify_cmd}");
    let instruction = format!(
        "Implement this project so ALL the existing tests pass: {TASK}\n\n\
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
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&worker.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some(verify_cmd.clone());
    agent_cfg.permission.frozen_paths = outcome.test_files.clone();
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;
    let sink = dc_core::FnSink(|e: &dc_core::AgentEvent| eprintln!("[A agent] {e:?}"));
    let report = dc_core::run_agent_observed(
        &worker,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        &ws_a,
        &agent_cfg,
        &sink,
    )
    .expect("Arm A agent ran");
    let a_green = report.verified == Some(true);
    eprintln!(
        "[A] RESULT finished={} verified={:?} steps={} files_built={:?}",
        report.finished,
        report.verified,
        report.steps,
        source_files(&ws_a)
    );

    // ===== ARM B: one raw completion to the same model =====
    // Same intent, no plan, no tools, no verify loop — just "write the code".
    let ws_b = fresh_ws(&cfg, "ab-raw");
    eprintln!(
        "\n===== ARM B (raw model, single shot) =====\nworkspace: {}",
        ws_b.display()
    );

    let prompt = format!(
        "Write a complete, runnable Python file `app.py` for this task. \
         Output ONLY the contents of app.py inside a single ```python code block — \
         no explanation.\n\nTask: {TASK} /no_think"
    );
    let mut req = GenerateRequest::new(vec![Message::user(prompt)]);
    req.max_tokens = 2048;
    let resp = worker.generate(&req).expect("Arm B generate");
    let code = extract_code_block(&resp.content);
    eprintln!(
        "[B] model returned {} chars; extracted {} chars of code",
        resp.content.len(),
        code.len()
    );
    std::fs::write(ws_b.join("app.py"), &code).expect("write Arm B app.py");
    // Judge Arm B by the SAME frozen tests Arm A produced — copy them in verbatim.
    for t in &outcome.test_files {
        let from = ws_a.join(t);
        let to = ws_b.join(t);
        if let Some(parent) = to.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(&from, &to).expect("copy frozen test to Arm B");
    }
    // Run the identical verify command in the identical sandbox.
    let b_report = dc_verify::run_verification_in(&cfg.sandbox(), &ws_b, &verify_cmd);
    let b_green = b_report.all_green();
    eprintln!("[B] verify:\n{}", b_report.observation());
    eprintln!("[B] files_built={:?}", source_files(&ws_b));

    // ===== VERDICT =====
    eprintln!("\n========== A/B VERDICT ==========");
    eprintln!("Task: {TASK}");
    eprintln!("Frozen tests (shared oracle): {:?}", outcome.test_files);
    eprintln!(
        "ARM A (dumb-coder pipeline): green={a_green}  steps={}",
        report.steps
    );
    eprintln!("ARM B (raw single completion): green={b_green}");
    eprintln!("=================================\n");

    // The test itself doesn't assert a winner (the model is nondeterministic) — it's
    // an observation harness. We only assert the comparison actually ran end to end.
    assert!(!outcome.test_files.is_empty());
}

// ---- helpers ----

fn fresh_ws(cfg: &UiConfig, tag: &str) -> std::path::PathBuf {
    let ws = cfg.run_workspace(&format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create workspace");
    ws
}

/// Pytest scoped to the named frozen files (mirror of session::combined_verify_command,
/// py-only since Arm B writes a Python app).
fn scoped_pytest(test_files: &[String]) -> String {
    let py: Vec<String> = test_files
        .iter()
        .filter(|f| f.ends_with(".py"))
        .map(|f| format!("'{f}'"))
        .collect();
    if py.is_empty() {
        "python -m pytest -q".to_string()
    } else {
        format!("python -m pytest -q {}", py.join(" "))
    }
}

/// Workspace-relative source files (exclude tests / dotdirs / caches) — what was built.
fn source_files(ws: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(ws) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.starts_with("__") || name.starts_with("test_") {
                continue;
            }
            if e.path().is_file() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// Pull the contents of the first ```...``` fenced block; if none, return the whole
/// response (the model sometimes emits bare code).
fn extract_code_block(s: &str) -> String {
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        // Skip an optional language tag on the same line.
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(end) = body.find("```") {
            return body[..end].trim_end().to_string();
        }
        return body.trim_end().to_string();
    }
    s.trim().to_string()
}
