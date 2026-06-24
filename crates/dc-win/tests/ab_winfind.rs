//! A/B win-finder: does the TDD pipeline beat a raw single completion on tasks rich
//! in edge cases a one-shot model tends to get subtly wrong?
//!
//! Methodology (fairer than ab_compare.rs): the oracle is a HAND-WRITTEN test suite
//! injected into BOTH arms — neither arm authors its own exam. Each arm produces
//! `app.py`; both are judged by the same frozen `test_app.py` in the same Docker
//! sandbox. Arm A gets the pipeline's plan as context but implements against the
//! injected tests via the agent loop; Arm B is one raw completion.
//!
//! Requires the live backend (`dc-qwen8b`, :11435) + sandbox image (`dumb-coder-pyenv`).
//!   cargo test -p dc-win --test ab_winfind -- --ignored --nocapture

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_win::config::UiConfig;

/// One A/B task: a prompt + the hand-written oracle that judges both arms. The oracle
/// covers the happy path AND the edge cases a one-shot model commonly ships wrong.
struct Task {
    name: &'static str,
    prompt: &'static str,
    oracle: &'static str, // contents of test_app.py
}

fn tasks() -> Vec<Task> {
    vec![
        // 1) Division: the divide-by-zero trap. A one-shot model usually returns 500
        //    (unhandled ZeroDivisionError) instead of a clean 400.
        Task {
            name: "divide-by-zero",
            prompt: "A Flask JSON API. POST /divide accepts JSON {\"a\":number,\"b\":number} \
                     and returns {\"result\": a/b} with 200. If b is 0, return \
                     {\"error\":\"division by zero\"} with status 400. The app must be \
                     importable as `from app import app`.",
            oracle: r#"from app import app

def test_divide_ok():
    c = app.test_client()
    r = c.post('/divide', json={'a': 10, 'b': 2})
    assert r.status_code == 200
    assert r.json == {'result': 5}

def test_divide_by_zero_is_400():
    c = app.test_client()
    r = c.post('/divide', json={'a': 1, 'b': 0})
    assert r.status_code == 400
    assert r.json == {'error': 'division by zero'}
"#,
        },
        // 2) Pagination clamping: ?page and ?limit must be clamped to valid ranges. A
        //    one-shot model usually trusts the query params verbatim.
        Task {
            name: "pagination-clamp",
            prompt: "A Flask JSON API. GET /items?page=P&limit=L returns \
                     {\"page\":P,\"limit\":L} with 200, but P is clamped to a minimum of 1 \
                     (any value < 1 becomes 1) and L is clamped to the range 1..=100 (values \
                     below 1 become 1, values above 100 become 100). Missing params default \
                     to page=1, limit=20. The app must be importable as `from app import app`.",
            oracle: r#"from app import app

def test_defaults():
    c = app.test_client()
    r = c.get('/items')
    assert r.status_code == 200
    assert r.json == {'page': 1, 'limit': 20}

def test_page_clamped_to_min_1():
    c = app.test_client()
    r = c.get('/items?page=-5&limit=10')
    assert r.json == {'page': 1, 'limit': 10}

def test_limit_clamped_to_max_100():
    c = app.test_client()
    r = c.get('/items?page=2&limit=9999')
    assert r.json == {'page': 2, 'limit': 100}

def test_limit_clamped_to_min_1():
    c = app.test_client()
    r = c.get('/items?page=1&limit=0')
    assert r.json == {'page': 1, 'limit': 1}
"#,
        },
        // 3) In-memory state + 404: a counter that persists across requests, and a
        //    missing resource must 404. One-shot models often skip the 404 path.
        Task {
            name: "stateful-counter",
            prompt: "A Flask JSON API with an in-memory store of named counters. \
                     POST /counter/<name>/incr increments the counter <name> (creating it at \
                     0 first if needed) and returns {\"name\":name,\"value\":new_value} with \
                     200. GET /counter/<name> returns {\"name\":name,\"value\":value} with 200 \
                     if it exists, or {\"error\":\"not found\"} with 404 if it was never \
                     incremented. The app must be importable as `from app import app`.",
            oracle: r#"from app import app

def test_unknown_counter_is_404():
    c = app.test_client()
    r = c.get('/counter/ghost')
    assert r.status_code == 404
    assert r.json == {'error': 'not found'}

def test_incr_creates_and_counts():
    c = app.test_client()
    r1 = c.post('/counter/hits/incr')
    assert r1.status_code == 200
    assert r1.json == {'name': 'hits', 'value': 1}
    r2 = c.post('/counter/hits/incr')
    assert r2.json == {'name': 'hits', 'value': 2}

def test_get_reflects_state():
    c = app.test_client()
    c.post('/counter/views/incr')
    c.post('/counter/views/incr')
    c.post('/counter/views/incr')
    r = c.get('/counter/views')
    assert r.status_code == 200
    assert r.json == {'name': 'views', 'value': 3}
"#,
        },
    ]
}

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn ab_winfind_across_tasks() {
    let cfg = UiConfig::default();
    let mut rows: Vec<(String, bool, bool, usize)> = Vec::new();

    for task in tasks() {
        eprintln!("\n############ TASK: {} ############", task.name);
        let (a_green, a_steps) = arm_pipeline(&cfg, &task);
        let b_green = arm_raw(&cfg, &task);
        eprintln!(
            "[{}] pipeline_green={a_green} (steps={a_steps})  raw_green={b_green}",
            task.name
        );
        rows.push((task.name.to_string(), a_green, b_green, a_steps));
    }

    eprintln!("\n================= WIN/LOSS TABLE =================");
    eprintln!(
        "{:<20} {:>10} {:>8} {:>8}",
        "task", "pipeline", "raw", "verdict"
    );
    for (name, a, b, _steps) in &rows {
        let verdict = match (a, b) {
            (true, false) => "PIPELINE WINS",
            (false, true) => "raw wins",
            (true, true) => "tie (both pass)",
            (false, false) => "tie (both fail)",
        };
        eprintln!(
            "{:<20} {:>10} {:>8} {:>8}",
            name,
            if *a { "green" } else { "RED" },
            if *b { "green" } else { "RED" },
            verdict
        );
    }
    let pipeline_wins = rows.iter().filter(|(_, a, b, _)| *a && !*b).count();
    let raw_wins = rows.iter().filter(|(_, a, b, _)| !*a && *b).count();
    eprintln!(
        "\nPipeline wins: {pipeline_wins}   Raw wins: {raw_wins}   (of {})",
        rows.len()
    );
    eprintln!("=================================================\n");

    // Observation harness — assert it ran, not who won (the model is nondeterministic).
    assert_eq!(rows.len(), 3);
}

/// Arm A: the pipeline plans, then the agent loop implements against the INJECTED
/// hand-written oracle (not a pipeline-generated test). Returns (green, steps).
fn arm_pipeline(cfg: &UiConfig, task: &Task) -> (bool, usize) {
    let ws = fresh_ws(cfg, &format!("win-pipe-{}", task.name));
    // Plan for context (specs/arch/etc.) — but we DON'T use its generated tests.
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let outcome = match dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        task.prompt,
        &ws,
        dc_workflow::ThinkPolicy::default(),
        &|_p, _c| {},
    ) {
        Ok(o) => Some(o),
        Err(e) => {
            eprintln!("[{}] pipeline plan failed: {e}", task.name);
            None
        }
    };

    // Inject the hand-written oracle as the frozen test (overwrite any generated one).
    // Remove pipeline-generated tests so only OUR oracle judges the run.
    remove_generated_tests(&ws);
    std::fs::write(ws.join("test_app.py"), task.oracle).expect("write oracle");

    let plan_ctx = outcome
        .as_ref()
        .map(|o| {
            o.state
                .approved()
                .iter()
                .map(|a| format!("=== {} ===\n{}", a.phase.title(), a.content))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();

    let instruction = format!(
        "Implement this project so ALL the existing tests pass: {}\n\n\
         The tests are already written and FROZEN — do not edit or delete any test file. \
         Read them to learn the exact contract, then write app.py. Use run_verification to \
         run the suite; keep editing until green, then finish.\n\nPlan:\n{plan_ctx}",
        task.prompt
    );
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&worker.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some("python -m pytest -q 'test_app.py'".to_string());
    agent_cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;
    let sink = dc_core::FnSink(|e: &dc_core::AgentEvent| eprintln!("[A {e:?}]"));
    match dc_core::run_agent_observed(
        &worker,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        &ws,
        &agent_cfg,
        &sink,
    ) {
        Ok(r) => (r.verified == Some(true), r.steps),
        Err(e) => {
            eprintln!("[{}] pipeline agent failed: {e}", task.name);
            (false, 0)
        }
    }
}

/// Arm B: one raw completion, judged by the same injected oracle.
fn arm_raw(cfg: &UiConfig, task: &Task) -> bool {
    let ws = fresh_ws(cfg, &format!("win-raw-{}", task.name));
    let worker = cfg.backend();
    let prompt = format!(
        "Write a complete, runnable Python file `app.py` for this task. Output ONLY the \
         contents of app.py inside a single ```python code block — no explanation.\n\n\
         Task: {} /no_think",
        task.prompt
    );
    let mut req = GenerateRequest::new(vec![Message::user(prompt)]);
    req.max_tokens = 2048;
    let code = match worker.generate(&req) {
        Ok(resp) => extract_code_block(&resp.content),
        Err(e) => {
            eprintln!("[{}] raw generate failed: {e}", task.name);
            return false;
        }
    };
    std::fs::write(ws.join("app.py"), &code).expect("write raw app.py");
    std::fs::write(ws.join("test_app.py"), task.oracle).expect("write oracle");
    let report =
        dc_verify::run_verification_in(&cfg.sandbox(), &ws, "python -m pytest -q 'test_app.py'");
    if !report.all_green() {
        eprintln!("[{} raw] {}", task.name, report.observation());
    }
    report.all_green()
}

// ---- helpers ----

fn fresh_ws(cfg: &UiConfig, tag: &str) -> std::path::PathBuf {
    let ws = cfg.run_workspace(&format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create workspace");
    ws
}

/// Remove any `test_*.py` the pipeline generated, so only the injected oracle judges.
fn remove_generated_tests(ws: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(ws) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("test_") && n.ends_with(".py") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn extract_code_block(s: &str) -> String {
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(end) = body.find("```") {
            return body[..end].trim_end().to_string();
        }
        return body.trim_end().to_string();
    }
    s.trim().to_string()
}
