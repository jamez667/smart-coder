//! Harder A/B: a stateful bank-ledger API with non-textbook invariants (atomic
//! transfer, overdraft rejection, non-positive guards, 404s) split across two files.
//! This is the regime where a single raw completion should finally ship a bug the
//! frozen tests catch — the pipeline iterates to green, the one-shot doesn't.
//!
//! Same methodology: a HAND-WRITTEN oracle judges both arms in the same Docker sandbox.
//!   cargo test -p dc-win --test ab_hard -- --ignored --nocapture

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_win::config::UiConfig;

const PROMPT: &str = "A Flask JSON bank API in TWO files. \
`bank.py` owns an in-memory ledger and pure functions (no Flask): \
`create_account()` -> new integer id (starting at 1); \
`balance(id)` -> int or None if the id doesn't exist; \
`deposit(id, amount)` -> True on success; `withdraw(id, amount)` -> True on success, \
False if the account lacks funds; `transfer(src, dst, amount)` -> True on success, \
False if src lacks funds — and a failed transfer must change NEITHER balance (atomic). \
All of deposit/withdraw/transfer must reject a non-positive amount (<= 0) by returning \
False and changing nothing. \
`app.py` imports bank and exposes JSON routes, all returning the named JSON below: \
POST /accounts -> {\"id\": id} 201; \
GET /accounts/<int:id> -> {\"id\": id, \"balance\": b} 200, or {\"error\": \"not found\"} 404; \
POST /accounts/<int:id>/deposit  {\"amount\": n} -> {\"balance\": b} 200, or {\"error\":\"invalid\"} 400; \
POST /accounts/<int:id>/withdraw {\"amount\": n} -> {\"balance\": b} 200, or {\"error\":\"invalid\"} 400 \
(invalid = unknown id, non-positive amount, or insufficient funds); \
POST /transfer {\"src\": a, \"dst\": b, \"amount\": n} -> {\"ok\": true} 200, or {\"error\":\"invalid\"} 400. \
`app` must be importable as `from app import app`. Keep the ledger logic in bank.py.";

/// The hand-written oracle — thorough on the traps: atomicity, overdraft, non-positive,
/// 404. Uses fresh accounts per assertion to avoid cross-test state coupling.
const ORACLE: &str = r#"from app import app

def _client():
    return app.test_client()

def _new(c):
    return c.post('/accounts').get_json()['id']

def test_create_and_get():
    c = _client()
    i = _new(c)
    r = c.get(f'/accounts/{i}')
    assert r.status_code == 200
    assert r.get_json() == {'id': i, 'balance': 0}

def test_get_unknown_is_404():
    c = _client()
    r = c.get('/accounts/999999')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_deposit_then_balance():
    c = _client()
    i = _new(c)
    r = c.post(f'/accounts/{i}/deposit', json={'amount': 100})
    assert r.status_code == 200
    assert r.get_json() == {'balance': 100}
    assert c.get(f'/accounts/{i}').get_json()['balance'] == 100

def test_deposit_non_positive_is_400():
    c = _client()
    i = _new(c)
    r = c.post(f'/accounts/{i}/deposit', json={'amount': 0})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'invalid'}
    assert c.get(f'/accounts/{i}').get_json()['balance'] == 0

def test_withdraw_insufficient_is_400_and_no_change():
    c = _client()
    i = _new(c)
    c.post(f'/accounts/{i}/deposit', json={'amount': 50})
    r = c.post(f'/accounts/{i}/withdraw', json={'amount': 80})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'invalid'}
    assert c.get(f'/accounts/{i}').get_json()['balance'] == 50

def test_withdraw_ok():
    c = _client()
    i = _new(c)
    c.post(f'/accounts/{i}/deposit', json={'amount': 50})
    r = c.post(f'/accounts/{i}/withdraw', json={'amount': 30})
    assert r.status_code == 200
    assert r.get_json() == {'balance': 20}

def test_transfer_ok_moves_funds():
    c = _client()
    a, b = _new(c), _new(c)
    c.post(f'/accounts/{a}/deposit', json={'amount': 100})
    r = c.post('/transfer', json={'src': a, 'dst': b, 'amount': 40})
    assert r.status_code == 200
    assert r.get_json() == {'ok': True}
    assert c.get(f'/accounts/{a}').get_json()['balance'] == 60
    assert c.get(f'/accounts/{b}').get_json()['balance'] == 40

def test_transfer_insufficient_is_atomic():
    # The hard one: a failed transfer must leave BOTH balances untouched.
    c = _client()
    a, b = _new(c), _new(c)
    c.post(f'/accounts/{a}/deposit', json={'amount': 30})
    c.post(f'/accounts/{b}/deposit', json={'amount': 5})
    r = c.post('/transfer', json={'src': a, 'dst': b, 'amount': 100})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'invalid'}
    assert c.get(f'/accounts/{a}').get_json()['balance'] == 30
    assert c.get(f'/accounts/{b}').get_json()['balance'] == 5

def test_transfer_non_positive_is_400():
    c = _client()
    a, b = _new(c), _new(c)
    c.post(f'/accounts/{a}/deposit', json={'amount': 10})
    r = c.post('/transfer', json={'src': a, 'dst': b, 'amount': -5})
    assert r.status_code == 400
    assert c.get(f'/accounts/{a}').get_json()['balance'] == 10
"#;

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn ab_hard_bank_ledger() {
    let cfg = UiConfig::default();
    let (a_green, a_steps, a_pass, a_total) = arm_pipeline(&cfg);
    let (b_green, b_pass, b_total) = arm_raw(&cfg);

    eprintln!("\n================ HARD A/B VERDICT ================");
    eprintln!("Task: stateful bank-ledger API (atomic transfer, overdraft, non-positive, 404)");
    eprintln!("ARM A (pipeline): green={a_green}  steps={a_steps}  tests={a_pass}/{a_total}");
    eprintln!("ARM B (raw):      green={b_green}  tests={b_pass}/{b_total}");
    let verdict = match (a_green, b_green) {
        (true, false) => "PIPELINE WINS",
        (false, true) => "raw wins",
        (true, true) => "tie (both pass)",
        (false, false) => "tie (both fail)",
    };
    eprintln!("VERDICT: {verdict}");
    eprintln!("=================================================\n");
    // Observation harness — we don't assert a winner.
    assert!(a_total > 0 || b_total > 0);
}

/// Arm A: pipeline plans, then the agent loop implements against the INJECTED oracle.
fn arm_pipeline(cfg: &UiConfig) -> (bool, usize, usize, usize) {
    let ws = fresh_ws(cfg, "hard-pipe");
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let outcome = dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        PROMPT,
        &ws,
        dc_workflow::ThinkPolicy::default(),
        &|_p, _c| {},
    )
    .ok();

    remove_generated_tests(&ws);
    std::fs::write(ws.join("test_app.py"), ORACLE).expect("write oracle");

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
        "Implement this project so ALL the existing tests pass: {PROMPT}\n\n\
         The tests are FROZEN — do not edit any test file. Read test_app.py for the exact \
         contract (status codes, JSON shapes, and the atomicity/overdraft/non-positive rules), \
         then create EVERY source file. Use run_verification; keep editing until green, then \
         finish.\n\nPlan:\n{plan_ctx}"
    );
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&worker.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some("python -m pytest -q 'test_app.py'".to_string());
    agent_cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;
    agent_cfg.max_steps = 60; // harder task → more iterations
    let sink = dc_core::FnSink(|e: &dc_core::AgentEvent| eprintln!("[A {e:?}]"));
    let report = dc_core::run_agent_observed(
        &worker,
        None,
        &registry,
        strategy.as_ref(),
        &instruction,
        &ws,
        &agent_cfg,
        &sink,
    );
    let (pass, total) = run_and_count(cfg, &ws);
    match report {
        Ok(r) => (r.verified == Some(true), r.steps, pass, total),
        Err(e) => {
            eprintln!("[pipeline] agent failed: {e}");
            (false, 0, pass, total)
        }
    }
}

/// Arm B: one raw completion (multi-file reply), judged by the same oracle.
fn arm_raw(cfg: &UiConfig) -> (bool, usize, usize) {
    let ws = fresh_ws(cfg, "hard-raw");
    let worker = cfg.backend();
    let prompt = format!(
        "Write ALL the source files for this task. The app spans MULTIPLE files. For EACH \
         file, output a fenced block whose info string is the path, like:\n\
         ```python path=app.py\n<contents>\n```\n\
         Output every file the task needs, nothing else.\n\nTask: {PROMPT} /no_think"
    );
    let mut req = GenerateRequest::new(vec![Message::user(prompt)]);
    req.max_tokens = 4096;
    let content = match worker.generate(&req) {
        Ok(resp) => resp.content,
        Err(e) => {
            eprintln!("[raw] generate failed: {e}");
            return (false, 0, 0);
        }
    };
    for (path, body) in parse_multifile(&content) {
        let p = ws.join(&path);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, body);
    }
    std::fs::write(ws.join("test_app.py"), ORACLE).expect("write oracle");
    let (pass, total) = run_and_count(cfg, &ws);
    (pass == total && total > 0, pass, total)
}

/// Run the oracle in the sandbox and return (passed, total) by parsing the report.
fn run_and_count(cfg: &UiConfig, ws: &std::path::Path) -> (usize, usize) {
    let report =
        dc_verify::run_verification_in(&cfg.sandbox(), ws, "python -m pytest -q 'test_app.py'");
    let obs = report.observation();
    eprintln!("[verify] {obs}");
    let passed = report.passed_count();
    let failed = report.failed().len();
    (passed, passed + failed)
}

// ---- helpers ----

fn fresh_ws(cfg: &UiConfig, tag: &str) -> std::path::PathBuf {
    let ws = cfg.run_workspace(&format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create workspace");
    ws
}

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

fn parse_multifile(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = s.lines().peekable();
    let mut pending_path: Option<String> = None;
    while let Some(line) = lines.next() {
        let t = line.trim();
        if let Some(rest) = t
            .strip_prefix("# file:")
            .or_else(|| t.strip_prefix("# path:"))
        {
            pending_path = Some(rest.trim().to_string());
            continue;
        }
        if let Some(info) = t.strip_prefix("```") {
            let path = info
                .split_whitespace()
                .find_map(|tok| tok.strip_prefix("path="))
                .map(|p| p.to_string())
                .or_else(|| pending_path.take());
            let mut body = String::new();
            for l in lines.by_ref() {
                if l.trim() == "```" {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            if let Some(p) = path {
                out.push((p, body.trim_end().to_string()));
            }
        }
    }
    out
}
