//! A difficulty ladder, both GREENFIELD (build from scratch) and EDIT (modify a seeded
//! codebase). Runs each rung through the pipeline vs a raw completion, judged by a
//! hand-written oracle in Docker, and prints a per-rung pass/total table so we find
//! WHERE the pipeline first fails.
//!
//!   cargo test -p dc-win --test ab_ladder -- --ignored --nocapture
//!
//! Greenfield rungs run BOTH arms (pipeline + raw). Edit rungs run the pipeline only
//! (a raw one-shot can't sensibly "edit" without the iterate loop), so the column reads
//! "n/a" for raw — the interesting question there is whether the agent can edit at all.

use std::collections::BTreeMap;

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_win::config::UiConfig;

/// A seeded file placed in the workspace before an EDIT task (path -> contents).
type Seed = &'static [(&'static str, &'static str)];

struct Rung {
    name: &'static str,
    kind: Kind,
    prompt: &'static str,
    oracle: &'static str,
    seed: Seed, // files pre-placed (edit rungs); empty for greenfield
}

#[derive(PartialEq)]
enum Kind {
    Greenfield,
    Edit,
}

fn rungs() -> Vec<Rung> {
    vec![
        // ===== GREENFIELD =====
        // G1: dependency resolution — topological order, reject cycles. Graph logic.
        Rung {
            name: "G1-task-scheduler",
            kind: Kind::Greenfield,
            prompt: "A Flask JSON API. POST /resolve with JSON {\"tasks\": {name: [deps...]}} \
                returns {\"order\": [...]} with 200 — a valid execution order where every task \
                comes after all its dependencies (topological sort). If the graph has a cycle, \
                return {\"error\": \"cycle\"} with 400. Unknown dependency names (a dep not \
                present as a task key) also return {\"error\": \"cycle\"} 400. `app` importable \
                as `from app import app`.",
            oracle: r#"from app import app

def post(body):
    return app.test_client().post('/resolve', json=body)

def test_linear_order():
    r = post({'tasks': {'a': [], 'b': ['a'], 'c': ['b']}})
    assert r.status_code == 200
    assert r.get_json()['order'] == ['a', 'b', 'c']

def test_order_respects_deps():
    r = post({'tasks': {'build': ['compile'], 'compile': [], 'test': ['build']}})
    order = r.get_json()['order']
    assert order.index('compile') < order.index('build') < order.index('test')

def test_cycle_is_400():
    r = post({'tasks': {'a': ['b'], 'b': ['a']}})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'cycle'}

def test_unknown_dep_is_400():
    r = post({'tasks': {'a': ['ghost']}})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'cycle'}
"#,
            seed: &[],
        },
        // G2: expression evaluator — precedence + parens. Real parsing.
        Rung {
            name: "G2-expr-eval",
            kind: Kind::Greenfield,
            prompt: "A Flask JSON API. POST /eval with JSON {\"expr\": \"...\"} evaluates an \
                integer arithmetic expression supporting + - * / (integer division, truncating \
                toward zero), parentheses, and unary minus, with normal precedence. Returns \
                {\"result\": n} 200. On a malformed expression or division by zero, return \
                {\"error\": \"bad expr\"} 400. Do NOT use python eval()/exec(); write a real \
                parser. `app` importable as `from app import app`.",
            oracle: r#"from app import app

def ev(expr):
    return app.test_client().post('/eval', json={'expr': expr})

def test_precedence():
    assert ev('2+3*4').get_json() == {'result': 14}

def test_parens():
    assert ev('(2+3)*4').get_json() == {'result': 20}

def test_unary_and_div():
    assert ev('-6/2').get_json() == {'result': -3}

def test_int_div_trunc():
    assert ev('7/2').get_json() == {'result': 3}

def test_div_zero_is_400():
    r = ev('1/0')
    assert r.status_code == 400
    assert r.get_json() == {'error': 'bad expr'}

def test_malformed_is_400():
    r = ev('2+*3')
    assert r.status_code == 400
    assert r.get_json() == {'error': 'bad expr'}
"#,
            seed: &[],
        },
        // G3: 3 files, referential integrity. Scale + cross-file invariant.
        Rung {
            name: "G3-three-file-refint",
            kind: Kind::Greenfield,
            prompt: "A Flask JSON API in THREE files. `store.py`: in-memory dicts + functions \
                for authors and books (a book has a title and an author_id). `service.py`: \
                imports store and enforces rules — creating a book requires an existing author; \
                deleting an author with any books must FAIL (referential integrity). `app.py`: \
                imports service and exposes routes: \
                POST /authors {\"name\":..} -> {\"id\":..} 201; \
                POST /books {\"title\":..,\"author_id\":..} -> {\"id\":..} 201 or {\"error\":\"no author\"} 400; \
                DELETE /authors/<int:id> -> {\"ok\":true} 200, or {\"error\":\"has books\"} 409 if it has books, \
                or {\"error\":\"not found\"} 404 if unknown. `app` importable as `from app import app`.",
            oracle: r#"from app import app

def c():
    return app.test_client()

def mk_author(cl, name='A'):
    return cl.post('/authors', json={'name': name}).get_json()['id']

def test_book_requires_author():
    cl = c()
    r = cl.post('/books', json={'title': 'X', 'author_id': 99999})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'no author'}

def test_create_book_ok():
    cl = c()
    a = mk_author(cl)
    r = cl.post('/books', json={'title': 'X', 'author_id': a})
    assert r.status_code == 201
    assert 'id' in r.get_json()

def test_delete_author_with_books_409():
    cl = c()
    a = mk_author(cl)
    cl.post('/books', json={'title': 'X', 'author_id': a})
    r = cl.delete(f'/authors/{a}')
    assert r.status_code == 409
    assert r.get_json() == {'error': 'has books'}

def test_delete_empty_author_ok():
    cl = c()
    a = mk_author(cl)
    r = cl.delete(f'/authors/{a}')
    assert r.status_code == 200
    assert r.get_json() == {'ok': True}

def test_delete_unknown_author_404():
    cl = c()
    r = cl.delete('/authors/123456')
    assert r.status_code == 404
"#,
            seed: &[],
        },
        // ===== EDIT =====
        // E1: ADD a feature to a working app, keeping existing behavior green.
        Rung {
            name: "E1-add-feature",
            kind: Kind::Edit,
            prompt: "The existing app.py implements GET /items (returns the list) and \
                POST /items (adds one). ADD a new route DELETE /items/<int:idx> that removes the \
                item at index idx (0-based) and returns {\"items\": [...]} 200, or \
                {\"error\":\"out of range\"} 404 if idx is invalid. Do not break the existing \
                routes. Edit app.py.",
            oracle: r#"from app import app

def c():
    return app.test_client()

def test_existing_still_works():
    cl = c()
    cl.post('/items', json={'item': 'a'})
    cl.post('/items', json={'item': 'b'})
    assert cl.get('/items').get_json()['items'][-2:] == ['a', 'b']

def test_delete_in_range():
    cl = c()
    cl.post('/items', json={'item': 'x'})
    before = cl.get('/items').get_json()['items']
    idx = len(before) - 1
    r = cl.delete(f'/items/{idx}')
    assert r.status_code == 200
    assert 'x' not in r.get_json()['items'] or before.count('x') > 1

def test_delete_out_of_range_404():
    cl = c()
    r = cl.delete('/items/999999')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'out of range'}
"#,
            seed: &[(
                "app.py",
                "from flask import Flask, request, jsonify\n\
                 app = Flask(__name__)\n\
                 _items = []\n\n\
                 @app.route('/items', methods=['GET'])\n\
                 def list_items():\n\
                 \x20   return jsonify({'items': _items}), 200\n\n\
                 @app.route('/items', methods=['POST'])\n\
                 def add_item():\n\
                 \x20   _items.append(request.get_json()['item'])\n\
                 \x20   return jsonify({'items': _items}), 201\n",
            )],
        },
        // E2: FIX a planted bug given a failing test, without breaking the passing ones.
        Rung {
            name: "E2-fix-bug",
            kind: Kind::Edit,
            prompt: "The existing app.py has a BUG in GET /page: pagination is off. The contract: \
                GET /page?n=N returns {\"page\": items} where items is the N-th page (1-based) of \
                PAGE_SIZE=3 over the list ['a'..'j'] (10 items). Page 1 = first 3, page 2 = next \
                3, etc. Page 4 = ['j']. Out-of-range or n<1 returns {\"page\": []}. Find and fix \
                the bug in app.py so the tests pass.",
            oracle: r#"from app import app

def pg(n):
    return app.test_client().get(f'/page?n={n}').get_json()['page']

def test_page1():
    assert pg(1) == ['a', 'b', 'c']

def test_page2():
    assert pg(2) == ['d', 'e', 'f']

def test_last_partial_page():
    assert pg(4) == ['j']

def test_out_of_range_empty():
    assert pg(5) == []
    assert pg(0) == []
"#,
            seed: &[(
                "app.py",
                "from flask import Flask, request, jsonify\n\
                 app = Flask(__name__)\n\
                 ITEMS = [chr(ord('a') + i) for i in range(10)]\n\
                 PAGE_SIZE = 3\n\n\
                 @app.route('/page')\n\
                 def page():\n\
                 \x20   n = int(request.args.get('n', 1))\n\
                 \x20   # BUG: uses n (not n-1) for the start offset, so page 1 skips the first page\n\
                 \x20   start = n * PAGE_SIZE\n\
                 \x20   return jsonify({'page': ITEMS[start:start + PAGE_SIZE]}), 200\n",
            )],
        },
        // E3: ROOT-CAUSE edit — a latent invariant bug the new test exposes; patching the
        // symptom won't pass all cases. The store allows negative stock; the new tests
        // require it never goes negative (clamp at the decrement, not just one route).
        Rung {
            name: "E3-root-cause",
            kind: Kind::Edit,
            prompt: "The existing inventory.py has `take(name, qty)` which subtracts qty from a \
                stock dict and `stock(name)`. There is a latent bug: it lets stock go negative. \
                Change the rule so a take that would drop stock below 0 instead takes NOTHING and \
                returns False (atomic), while a valid take returns True. Also app.py exposes \
                POST /take {\"name\":..,\"qty\":..} -> {\"stock\": s} 200 on success, or \
                {\"error\":\"insufficient\"} 400 on failure. Fix the root rule in inventory.py and \
                wire the route. Edit both files as needed.",
            oracle: r#"from app import app
import inventory

def test_take_ok():
    inventory.STOCK['widget'] = 10
    assert inventory.take('widget', 4) is True
    assert inventory.stock('widget') == 6

def test_take_too_much_is_atomic():
    inventory.STOCK['gadget'] = 5
    assert inventory.take('gadget', 9) is False
    assert inventory.stock('gadget') == 5  # unchanged

def test_route_success():
    inventory.STOCK['bolt'] = 8
    r = app.test_client().post('/take', json={'name': 'bolt', 'qty': 3})
    assert r.status_code == 200
    assert r.get_json() == {'stock': 5}

def test_route_insufficient_400():
    inventory.STOCK['nut'] = 2
    r = app.test_client().post('/take', json={'name': 'nut', 'qty': 10})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'insufficient'}
    assert inventory.stock('nut') == 2
"#,
            seed: &[
                (
                    "inventory.py",
                    "STOCK = {}\n\n\
                     def take(name, qty):\n\
                     \x20   # BUG: no floor — stock can go negative\n\
                     \x20   STOCK[name] = STOCK.get(name, 0) - qty\n\
                     \x20   return True\n\n\
                     def stock(name):\n\
                     \x20   return STOCK.get(name, 0)\n",
                ),
                (
                    "app.py",
                    "from flask import Flask, request, jsonify\n\
                     import inventory\n\
                     app = Flask(__name__)\n\n\
                     # TODO: add POST /take\n",
                ),
            ],
        },
    ]
}

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn ab_difficulty_ladder() {
    let mut cfg = UiConfig::default();
    // Model-swap knob (no code edit needed to benchmark a different backend):
    //   DC_BASE_URL  e.g. http://localhost:11436/v1   DC_MODEL  e.g. gemma4-31b
    //   DC_SUFFIX    think-suppression suffix; set to empty to clear /no_think (Gemma)
    if let Ok(u) = std::env::var("DC_BASE_URL") {
        cfg.base_url = u;
    }
    if let Ok(m) = std::env::var("DC_MODEL") {
        cfg.model = m;
    }
    if let Ok(s) = std::env::var("DC_SUFFIX") {
        cfg.system_suffix = if s.is_empty() { None } else { Some(s) };
    }
    eprintln!(
        "BACKEND: {} model={} suffix={:?}",
        cfg.base_url, cfg.model, cfg.system_suffix
    );
    // name -> (kind, pipeline pass/total + green, raw pass/total + green-or-na)
    let mut rows: Vec<Row> = Vec::new();

    for r in rungs() {
        eprintln!(
            "\n############ RUNG: {} ({:?}) ############",
            r.name,
            kind_str(&r.kind)
        );
        let (p_green, p_steps, p_pass, p_total) = arm_pipeline(&cfg, &r);
        let raw = if r.kind == Kind::Greenfield {
            let (g, pass, total) = arm_raw(&cfg, &r);
            Some((g, pass, total))
        } else {
            None
        };
        eprintln!(
            "[{}] pipeline green={p_green} steps={p_steps} {p_pass}/{p_total}  raw={:?}",
            r.name, raw
        );
        rows.push(Row {
            name: r.name.to_string(),
            kind: kind_str(&r.kind),
            p_green,
            p_pass,
            p_total,
            raw,
        });
    }

    eprintln!("\n==================== LADDER RESULTS ====================");
    eprintln!(
        "{:<22} {:<10} {:>12} {:>12}",
        "rung", "kind", "pipeline", "raw"
    );
    for row in &rows {
        let pipe = format!(
            "{} {}/{}",
            if row.p_green { "WIN" } else { "fail" },
            row.p_pass,
            row.p_total
        );
        let raw = match &row.raw {
            Some((g, pass, total)) => {
                format!("{} {}/{}", if *g { "ok" } else { "fail" }, pass, total)
            }
            None => "n/a".to_string(),
        };
        eprintln!("{:<22} {:<10} {:>12} {:>12}", row.name, row.kind, pipe, raw);
    }
    let first_fail = rows.iter().find(|r| !r.p_green);
    match first_fail {
        Some(r) => eprintln!("\nPipeline FIRST FAILED at: {} ({})", r.name, r.kind),
        None => eprintln!("\nPipeline passed ALL rungs."),
    }
    eprintln!("=======================================================\n");
    assert!(!rows.is_empty());
}

struct Row {
    name: String,
    kind: &'static str,
    p_green: bool,
    p_pass: usize,
    p_total: usize,
    raw: Option<(bool, usize, usize)>,
}

fn kind_str(k: &Kind) -> &'static str {
    match k {
        Kind::Greenfield => "greenfield",
        Kind::Edit => "edit",
    }
}

/// Arm A: pipeline. For greenfield, plan + agent loop. For edit, seed the files first
/// and run the agent loop directly with the edit instruction (skip the planner — there's
/// nothing to architect, just an edit against existing code + the new test).
fn arm_pipeline(cfg: &UiConfig, rung: &Rung) -> (bool, usize, usize, usize) {
    let ws = fresh_ws(cfg, &format!("ladder-pipe-{}", rung.name));
    seed_files(&ws, rung.seed);

    let worker = cfg.backend();
    let plan_ctx = if rung.kind == Kind::Greenfield {
        let orchestrator = cfg.orchestrator();
        dc_workflow::run_workflow(
            &orchestrator,
            &worker,
            rung.prompt,
            &ws,
            dc_workflow::ThinkPolicy::default(),
            &|_p, _c| {},
        )
        .ok()
        .map(|o| {
            // The planner may have written stray source files; the agent will overwrite.
            o.state
                .approved()
                .iter()
                .map(|a| format!("=== {} ===\n{}", a.phase.title(), a.content))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
    } else {
        String::new()
    };

    // Remove any planner-written tests, then inject the oracle as the frozen contract.
    remove_generated_tests(&ws);
    std::fs::write(ws.join("test_app.py"), rung.oracle).expect("write oracle");

    let verb = if rung.kind == Kind::Edit {
        "Edit the existing source files so ALL the tests pass"
    } else {
        "Implement this project so ALL the tests pass"
    };
    let plan_block = if plan_ctx.is_empty() {
        String::new()
    } else {
        format!("\n\nPlan:\n{plan_ctx}")
    };
    let instruction = format!(
        "{verb}: {}\n\n\
         The tests are FROZEN — do not edit any test file. Read test_app.py for the exact \
         contract. {} Use run_verification; keep editing until green, then finish.{plan_block}",
        rung.prompt,
        if rung.kind == Kind::Edit {
            "The source files already exist — read them first, then modify them."
        } else {
            "Create every source file the task needs."
        }
    );

    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&worker.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some("python -m pytest -q 'test_app.py'".to_string());
    agent_cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;
    agent_cfg.max_steps = 60;
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
            eprintln!("[{}] agent failed: {e}", rung.name);
            (false, 0, pass, total)
        }
    }
}

/// Arm B: one raw completion (greenfield only). Seeds nothing.
fn arm_raw(cfg: &UiConfig, rung: &Rung) -> (bool, usize, usize) {
    let ws = fresh_ws(cfg, &format!("ladder-raw-{}", rung.name));
    let worker = cfg.backend();
    let prompt = format!(
        "Write ALL the source files for this task. For EACH file, output a fenced block whose \
         info string is the path, like:\n```python path=app.py\n<contents>\n```\nOutput every \
         file the task needs, nothing else.\n\nTask: {} /no_think",
        rung.prompt
    );
    let mut req = GenerateRequest::new(vec![Message::user(prompt)]);
    req.max_tokens = 4096;
    let content = match worker.generate(&req) {
        Ok(resp) => resp.content,
        Err(e) => {
            eprintln!("[{}] raw generate failed: {e}", rung.name);
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
    std::fs::write(ws.join("test_app.py"), rung.oracle).expect("write oracle");
    let (pass, total) = run_and_count(cfg, &ws);
    (pass == total && total > 0, pass, total)
}

fn run_and_count(cfg: &UiConfig, ws: &std::path::Path) -> (usize, usize) {
    let report =
        dc_verify::run_verification_in(&cfg.sandbox(), ws, "python -m pytest -q 'test_app.py'");
    eprintln!("[verify] {}", report.observation());
    let passed = report.passed_count();
    (passed, passed + report.failed().len())
}

// ---- helpers ----

fn seed_files(ws: &std::path::Path, seed: Seed) {
    for (path, body) in seed {
        let p = ws.join(path);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&p, body).expect("seed file");
    }
}

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
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
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
                if seen.insert(p.clone(), ()).is_none() {
                    out.push((p, body.trim_end().to_string()));
                }
            }
        }
    }
    out
}
