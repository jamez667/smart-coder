//! A/B on MULTI-FILE apps — the regime where a single raw completion should break
//! down (it can't hold cross-file contracts) but the pipeline's plan + iterate-to-green
//! loop can. Same methodology as ab_winfind: a HAND-WRITTEN oracle judges both arms in
//! the same Docker sandbox, so neither authors its own exam.
//!
//!   cargo test -p dc-win --test ab_multifile -- --ignored --nocapture

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_win::config::UiConfig;

/// A multi-file task: the prompt names ≥2 source files with a contract across them.
/// `oracle` is the frozen test; it exercises the behavior that depends on BOTH files.
struct Task {
    name: &'static str,
    prompt: &'static str,
    oracle: &'static str,
}

fn tasks() -> Vec<Task> {
    vec![
        // 1) app.py imports a helper module. The contract is the import boundary: app.py
        //    must call validators.validate_username with the exact rules the helper owns.
        Task {
            name: "app+helper-module",
            prompt: "A Flask JSON API in TWO files. \
                `validators.py` defines `validate_username(name)` returning True iff name is \
                3..=20 chars and all characters are alphanumeric or underscore, else False. \
                `app.py` imports it and exposes POST /register with JSON {\"username\":str}: if \
                valid it returns {\"ok\": true} with 200, else {\"ok\": false, \"error\": \
                \"invalid username\"} with 400. `app` must be importable as `from app import app`. \
                Keep the validation logic in validators.py, not app.py.",
            oracle: r#"from app import app
from validators import validate_username

def test_helper_rules():
    assert validate_username('ab') is False          # too short
    assert validate_username('a' * 21) is False       # too long
    assert validate_username('good_name9') is True
    assert validate_username('bad name') is False     # space not allowed

def test_register_valid():
    c = app.test_client()
    r = c.post('/register', json={'username': 'alice_99'})
    assert r.status_code == 200
    assert r.get_json() == {'ok': True}

def test_register_invalid():
    c = app.test_client()
    r = c.post('/register', json={'username': 'no'})
    assert r.status_code == 400
    assert r.get_json() == {'ok': False, 'error': 'invalid username'}
"#,
        },
        // 2) app.py + db.py: state lives in a separate module that app.py mutates. The
        //    cross-file contract is the shared store surviving across requests.
        Task {
            name: "app+db-module",
            prompt: "A Flask JSON API in TWO files. \
                `db.py` holds an in-memory dict and defines `add(item)` (appends a string item, \
                returns its new integer id starting at 1) and `all_items()` (returns the list of \
                items in insertion order). `app.py` imports db and exposes POST /items with JSON \
                {\"item\":str} returning {\"id\": id} with 201, and GET /items returning \
                {\"items\": [...]} with 200. `app` must be importable as `from app import app`. \
                Keep the storage in db.py.",
            oracle: r#"from app import app
import db

def test_add_then_list_via_db():
    # db state is shared; use a fresh-ish view by checking relative behavior
    start = len(db.all_items())
    id1 = db.add('x')
    assert id1 == start + 1
    assert db.all_items()[-1] == 'x'

def test_post_returns_id_and_get_lists():
    c = app.test_client()
    r1 = c.post('/items', json={'item': 'apple'})
    assert r1.status_code == 201
    first_id = r1.get_json()['id']
    r2 = c.post('/items', json={'item': 'banana'})
    assert r2.get_json()['id'] == first_id + 1
    g = c.get('/items')
    assert g.status_code == 200
    items = g.get_json()['items']
    assert items[-2:] == ['apple', 'banana']
"#,
        },
        // 3) app.py + a Jinja template. The route must RENDER templates/index.html, not
        //    inline HTML — a one-shot model often forgets the separate template file.
        Task {
            name: "app+jinja-template",
            prompt: "A Flask app in TWO files. `templates/index.html` is a Jinja template that \
                renders a page containing an <h1> with the text passed in as `title` and a <ul> \
                with one <li> per name in the `names` list. `app.py` exposes GET / which calls \
                render_template('index.html', title='Welcome', names=['Ann','Bob']) and returns \
                the HTML with 200. `app` must be importable as `from app import app`.",
            oracle: r#"from app import app

def test_root_renders_template():
    c = app.test_client()
    r = c.get('/')
    assert r.status_code == 200
    html = r.get_data(as_text=True)
    assert '<h1>Welcome</h1>' in html
    assert '<li>Ann</li>' in html
    assert '<li>Bob</li>' in html
"#,
        },
    ]
}

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn ab_multifile_across_tasks() {
    let cfg = UiConfig::default();
    let mut rows: Vec<(String, bool, bool, usize, Vec<String>, Vec<String>)> = Vec::new();

    for task in tasks() {
        eprintln!("\n############ TASK: {} ############", task.name);
        let (a_green, a_steps, a_files) = arm_pipeline(&cfg, &task);
        let (b_green, b_files) = arm_raw(&cfg, &task);
        eprintln!(
            "[{}] pipeline_green={a_green} (steps={a_steps}, files={a_files:?})  \
             raw_green={b_green} (files={b_files:?})",
            task.name
        );
        rows.push((
            task.name.into(),
            a_green,
            b_green,
            a_steps,
            a_files,
            b_files,
        ));
    }

    eprintln!("\n============== MULTI-FILE WIN/LOSS ==============");
    eprintln!(
        "{:<22} {:>9} {:>6} {:>16}",
        "task", "pipeline", "raw", "verdict"
    );
    for (name, a, b, _s, _af, _bf) in &rows {
        let verdict = match (a, b) {
            (true, false) => "PIPELINE WINS",
            (false, true) => "raw wins",
            (true, true) => "tie (both pass)",
            (false, false) => "tie (both fail)",
        };
        eprintln!(
            "{:<22} {:>9} {:>6} {:>16}",
            name,
            if *a { "green" } else { "RED" },
            if *b { "green" } else { "RED" },
            verdict
        );
    }
    let pw = rows.iter().filter(|(_, a, b, _, _, _)| *a && !*b).count();
    let rw = rows.iter().filter(|(_, a, b, _, _, _)| !*a && *b).count();
    eprintln!(
        "\nPipeline wins: {pw}   Raw wins: {rw}   (of {})",
        rows.len()
    );
    eprintln!("================================================\n");
    assert_eq!(rows.len(), 3);
}

/// Arm A: pipeline plans, then the agent loop implements against the INJECTED oracle.
fn arm_pipeline(cfg: &UiConfig, task: &Task) -> (bool, usize, Vec<String>) {
    let ws = fresh_ws(cfg, &format!("mf-pipe-{}", task.name));
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();
    let outcome = dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        task.prompt,
        &ws,
        dc_workflow::ThinkPolicy::default(),
        &|_p, _c| {},
    )
    .ok();

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
         The tests are FROZEN — do not edit any test file. Read test_app.py for the exact \
         contract, then create EVERY source file the task needs (it spans multiple files). \
         Use run_verification; keep editing until green, then finish.\n\nPlan:\n{plan_ctx}",
        task.prompt
    );
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&worker.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some("python -m pytest -q 'test_app.py'".to_string());
    agent_cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;
    // Give the single agent more room — multi-file is more steps than one file.
    agent_cfg.max_steps = 40;
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
        Ok(r) => (r.verified == Some(true), r.steps, source_files(&ws)),
        Err(e) => {
            eprintln!("[{}] pipeline agent failed: {e}", task.name);
            (false, 0, source_files(&ws))
        }
    }
}

/// Arm B: one raw completion. The model must emit ALL files in one shot — we parse a
/// multi-file reply (```python path=... blocks or `# file: path` markers).
fn arm_raw(cfg: &UiConfig, task: &Task) -> (bool, Vec<String>) {
    let ws = fresh_ws(cfg, &format!("mf-raw-{}", task.name));
    let worker = cfg.backend();
    let prompt = format!(
        "Write ALL the source files for this task. The app spans MULTIPLE files. For EACH \
         file, output a fenced block whose info string is the path, like:\n\
         ```python path=app.py\n<contents>\n```\n\
         and for a template:\n```html path=templates/index.html\n<contents>\n```\n\
         Output every file the task needs, nothing else.\n\nTask: {} /no_think",
        task.prompt
    );
    let mut req = GenerateRequest::new(vec![Message::user(prompt)]);
    req.max_tokens = 3072;
    let content = match worker.generate(&req) {
        Ok(resp) => resp.content,
        Err(e) => {
            eprintln!("[{}] raw generate failed: {e}", task.name);
            return (false, vec![]);
        }
    };
    let files = parse_multifile(&content);
    if files.is_empty() {
        eprintln!(
            "[{} raw] no files parsed from reply ({} chars)",
            task.name,
            content.len()
        );
    }
    for (path, body) in &files {
        let p = ws.join(path);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, body);
    }
    std::fs::write(ws.join("test_app.py"), task.oracle).expect("write oracle");
    let report =
        dc_verify::run_verification_in(&cfg.sandbox(), &ws, "python -m pytest -q 'test_app.py'");
    if !report.all_green() {
        eprintln!("[{} raw] {}", task.name, report.observation());
    }
    (report.all_green(), source_files(&ws))
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

/// Source files actually built (recursive, workspace-relative, excluding tests/caches).
fn source_files(ws: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(ws, ws, &mut out);
    out.sort();
    out
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.starts_with("__") {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if !name.starts_with("test_") {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// Parse a multi-file reply: fenced blocks whose info string carries `path=<p>`, or
/// `# file: <p>` markers before a block. Returns (path, contents) pairs.
fn parse_multifile(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = s.lines().peekable();
    let mut pending_path: Option<String> = None;
    while let Some(line) = lines.next() {
        let t = line.trim();
        // `# file: path` / `# path: app.py` style marker.
        if let Some(rest) = t
            .strip_prefix("# file:")
            .or_else(|| t.strip_prefix("# path:"))
            .or_else(|| t.strip_prefix("// file:"))
        {
            pending_path = Some(rest.trim().to_string());
            continue;
        }
        // Opening fence, possibly `lang path=app.py`.
        if let Some(info) = t.strip_prefix("```") {
            let path = info
                .split_whitespace()
                .find_map(|tok| tok.strip_prefix("path="))
                .map(|p| p.to_string())
                .or_else(|| pending_path.take());
            // Collect until the closing fence.
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
