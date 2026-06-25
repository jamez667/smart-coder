//! The SCALE LADDER — find the real ceiling of the coder on MANY-FILE projects, and on
//! the harder skill of EXTENDING code it just wrote. Each rung is TWO passes against the
//! SAME workspace:
//!   PASS 1 (build):  plan + agent loop vs oracle_v1 (greenfield server + client).
//!   PASS 2 (extend): overwrite test_app.py with oracle_v2 (a SUPERSET — all v1 tests plus
//!                    new-feature tests), re-run the agent plan-free with an "add this
//!                    feature, keep the old tests green" instruction — it must READ the
//!                    code it built and extend it.
//! A rung WINS only if BOTH passes are green. We climb file-count + cross-file contract
//! complexity deliberately PAST the ceiling — some 0/N is the point.
//!
//!   cargo test -p dc-win --test ab_scale -- --ignored --nocapture
//!
//! The "client" is a server-rendered HTML template + a static .js served by Flask, so the
//! whole app is verified through ONE pytest oracle (the page via get_data(as_text=True),
//! the API via get_json()) — no dual pytest/vitest runner. Near the ceiling there is real
//! variance; run 2-3x and read the modal result. DC_BASE_URL/DC_MODEL/DC_SUFFIX swap the
//! backend without a code edit.

use dc_model::ModelBackend;
use dc_win::config::{source_files, UiConfig};

/// One scale rung: a greenfield BUILD (oracle_v1) followed by an EXTEND (oracle_v2, a
/// superset). `file_count_hint` is the rough size the prompt names, for the table.
struct Rung {
    name: &'static str,
    prompt: &'static str,        // v1 greenfield BUILD prompt — names every file
    extend_prompt: &'static str, // v2 EXTEND instruction — "the files exist, read them"
    oracle_v1: &'static str,     // frozen pytest contract for pass 1
    oracle_v2: &'static str,     // superset: all v1 tests + new-feature tests
    file_count_hint: usize,
}

/// (green, agent steps, pytest pass count, pytest total) for one pass.
type Pass = (bool, usize, usize, usize);

struct Row {
    name: String,
    file_count_hint: usize,
    v1: Pass,
    v2: Pass,
    /// The NEW sequential per-file BUILD pass, for the A/B against the whole-task `v1`.
    seq_build: Pass,
    seq_files: usize,
}

/// ab_multifile used 40 steps for 2 files; ab_ladder bumped to 60 for 3-file/edit rungs.
/// These rungs are 4/6/8 files with blueprints + a template + a static file, so give the
/// BUILD real head-room while still terminating a thrasher. EXTEND touches fewer files but
/// must read-then-extend without regressing v1, so it sits between the two.
const BUILD_STEPS: usize = 80;
const EXTEND_STEPS: usize = 50;

fn rungs() -> Vec<Rung> {
    vec![
        // ===== S1: URL shortener (~4 files) =====
        Rung {
            name: "S1-url-shortener",
            file_count_hint: 4,
            prompt: "A Flask URL-shortener in FOUR files, app importable as `from app import app`. \
                `store.py`: an in-memory module with `save(url)` -> a short code string \
                (deterministic, url-safe, e.g. base62 of an incrementing id), `resolve(code)` -> \
                the url or None, and a dict-backed store. `app.py`: imports store and exposes \
                POST /shorten {\"url\":str} -> {\"code\":code} 201; \
                GET /<code> -> 302 redirect whose Location is the stored url, or \
                {\"error\":\"not found\"} 404 if the code is unknown; \
                GET / -> renders templates/index.html (200) with a <form> to submit a url and a \
                <script> tag whose src is the static file static/app.js. \
                `templates/index.html`: a Jinja page with an <h1>Shorten</h1>, a <form> \
                containing an <input name=\"url\">, and <script src=\"/static/app.js\"></script>. \
                `static/app.js`: plain browser JS that fetches POST /shorten with the form's url \
                and shows the returned code (fetch + DOM, no npm). Keep storage in store.py. \
                Write plain `def` route handlers, never async def.",
            extend_prompt: "ADD a hit-counter to the existing shortener. The files (store.py, \
                app.py, templates/index.html, static/app.js) ALREADY EXIST — read them first, \
                then extend them. store.py: every successful GET /<code> redirect increments a \
                per-code click count; add `clicks(code)` -> int (0 for unknown). app.py: add \
                GET /stats/<code> -> {\"code\":code,\"clicks\":n} 200, or {\"error\":\"not found\"} \
                404 for an unknown code. templates/index.html: add an element with id=\"stats\" \
                where the page can show a code's clicks. Keep ALL existing routes and behavior \
                working — do not break the redirect or the 201 on /shorten. Plain def only.",
            oracle_v1: r#"from app import app

def c():
    return app.test_client()

def test_root_serves_page():
    r = c().get('/')
    assert r.status_code == 200
    html = r.get_data(as_text=True)
    assert '<h1>Shorten</h1>' in html
    assert 'name="url"' in html
    assert '/static/app.js' in html

def test_shorten_returns_code():
    r = c().post('/shorten', json={'url': 'https://example.com/a'})
    assert r.status_code == 201
    body = r.get_json()
    assert set(body.keys()) == {'code'}
    assert isinstance(body['code'], str) and body['code'] != ''

def test_redirect_to_original():
    cl = c()
    code = cl.post('/shorten', json={'url': 'https://example.com/b'}).get_json()['code']
    r = cl.get(f'/{code}')
    assert r.status_code == 302
    assert r.headers['Location'] == 'https://example.com/b'

def test_unknown_code_404():
    r = c().get('/zzzzzz')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_static_app_js_is_served():
    r = c().get('/static/app.js')
    assert r.status_code == 200
    assert 'fetch' in r.get_data(as_text=True)
"#,
            oracle_v2: r#"from app import app

def c():
    return app.test_client()

def test_root_serves_page():
    r = c().get('/')
    assert r.status_code == 200
    html = r.get_data(as_text=True)
    assert '<h1>Shorten</h1>' in html
    assert 'name="url"' in html
    assert '/static/app.js' in html

def test_shorten_returns_code():
    r = c().post('/shorten', json={'url': 'https://example.com/a'})
    assert r.status_code == 201
    body = r.get_json()
    assert set(body.keys()) == {'code'}
    assert isinstance(body['code'], str) and body['code'] != ''

def test_redirect_to_original():
    cl = c()
    code = cl.post('/shorten', json={'url': 'https://example.com/b'}).get_json()['code']
    r = cl.get(f'/{code}')
    assert r.status_code == 302
    assert r.headers['Location'] == 'https://example.com/b'

def test_unknown_code_404():
    r = c().get('/zzzzzz')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_static_app_js_is_served():
    r = c().get('/static/app.js')
    assert r.status_code == 200
    assert 'fetch' in r.get_data(as_text=True)

def test_stats_starts_at_zero():
    cl = c()
    code = cl.post('/shorten', json={'url': 'https://example.com/c'}).get_json()['code']
    r = cl.get(f'/stats/{code}')
    assert r.status_code == 200
    assert r.get_json() == {'code': code, 'clicks': 0}

def test_stats_counts_redirects():
    cl = c()
    code = cl.post('/shorten', json={'url': 'https://example.com/d'}).get_json()['code']
    cl.get(f'/{code}')
    cl.get(f'/{code}')
    assert cl.get(f'/stats/{code}').get_json() == {'code': code, 'clicks': 2}

def test_stats_unknown_404():
    r = c().get('/stats/nope123')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_page_has_stats_element():
    assert 'id="stats"' in c().get('/').get_data(as_text=True)
"#,
        },
        // ===== S2: TODO board (~6 files) =====
        Rung {
            name: "S2-todo-board",
            file_count_hint: 6,
            prompt: "A Flask in-memory TODO board in SIX files, app importable as \
                `from app import app`. `store.py`: a dict-backed task store with `add(title)` -> \
                the new task dict {\"id\":int,\"title\":str,\"done\":bool} (ids start at 1, done \
                defaults false), `all()` -> the list of task dicts in insertion order, `get(id)` \
                -> task or None, `set_done(id,bool)` -> task or None. `service.py`: imports store \
                and owns the operations create_task(title), list_tasks(), complete_task(id) (sets \
                done true, returns task or None). `routes.py`: a Flask Blueprint named 'tasks' \
                that imports service and defines \
                POST /tasks {\"title\":str} -> the task dict 201; \
                GET /tasks -> {\"tasks\":[...]} 200; \
                POST /tasks/<int:id>/complete -> the task dict 200, or {\"error\":\"not found\"} \
                404. `app.py`: creates the Flask app, registers the blueprint, and adds GET / -> \
                renders templates/board.html (200) listing every task title in a <ul><li>, with \
                <script src=\"/static/board.js\"></script>. `templates/board.html`: <h1>Board</h1>, \
                the <ul> of task titles, and the script tag. `static/board.js`: plain JS that GETs \
                /tasks and renders them (fetch + DOM). Keep storage in store.py and rules in \
                service.py. Write plain `def` route handlers, never async def.",
            extend_prompt: "ADD filtering and delete to the existing board. ALL files (store.py, \
                service.py, routes.py, app.py, templates/board.html, static/board.js) ALREADY \
                EXIST — read them first, then extend. GET /tasks must accept an optional \
                ?status=active|done query: 'active' returns only done==false tasks, 'done' returns \
                only done==true tasks, absent/any other value returns ALL (unchanged). Add \
                DELETE /tasks/<int:id> -> {\"deleted\":id} 200, or {\"error\":\"not found\"} 404. \
                Add the supporting store/service functions (e.g. remove(id), delete_task(id)). \
                Keep ALL existing CRUD behavior green. Plain def only.",
            oracle_v1: r#"from app import app

def c():
    return app.test_client()

def mk(cl, title):
    return cl.post('/tasks', json={'title': title}).get_json()

def test_create_returns_task():
    cl = c()
    t = mk(cl, 'write spec')
    assert t['title'] == 'write spec'
    assert t['done'] is False
    assert isinstance(t['id'], int) and t['id'] >= 1
    assert set(t.keys()) == {'id', 'title', 'done'}

def test_ids_increment_and_list_order():
    cl = c()
    a = mk(cl, 'a')
    b = mk(cl, 'b')
    assert b['id'] == a['id'] + 1
    titles = [t['title'] for t in cl.get('/tasks').get_json()['tasks']]
    assert titles[-2:] == ['a', 'b']

def test_complete_sets_done():
    cl = c()
    t = mk(cl, 'do it')
    r = cl.post(f"/tasks/{t['id']}/complete")
    assert r.status_code == 200
    assert r.get_json() == {'id': t['id'], 'title': 'do it', 'done': True}

def test_complete_unknown_404():
    r = c().post('/tasks/999999/complete')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_board_lists_titles():
    cl = c()
    mk(cl, 'alpha')
    mk(cl, 'beta')
    html = cl.get('/').get_data(as_text=True)
    assert '<h1>Board</h1>' in html
    assert '<li>alpha</li>' in html
    assert '<li>beta</li>' in html
    assert '/static/board.js' in html
"#,
            oracle_v2: r#"from app import app

def c():
    return app.test_client()

def mk(cl, title):
    return cl.post('/tasks', json={'title': title}).get_json()

def test_create_returns_task():
    cl = c()
    t = mk(cl, 'write spec')
    assert t['title'] == 'write spec'
    assert t['done'] is False
    assert isinstance(t['id'], int) and t['id'] >= 1
    assert set(t.keys()) == {'id', 'title', 'done'}

def test_ids_increment_and_list_order():
    cl = c()
    a = mk(cl, 'a')
    b = mk(cl, 'b')
    assert b['id'] == a['id'] + 1
    titles = [t['title'] for t in cl.get('/tasks').get_json()['tasks']]
    assert titles[-2:] == ['a', 'b']

def test_complete_sets_done():
    cl = c()
    t = mk(cl, 'do it')
    r = cl.post(f"/tasks/{t['id']}/complete")
    assert r.status_code == 200
    assert r.get_json() == {'id': t['id'], 'title': 'do it', 'done': True}

def test_complete_unknown_404():
    r = c().post('/tasks/999999/complete')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_board_lists_titles():
    cl = c()
    mk(cl, 'alpha')
    mk(cl, 'beta')
    html = cl.get('/').get_data(as_text=True)
    assert '<h1>Board</h1>' in html
    assert '<li>alpha</li>' in html
    assert '<li>beta</li>' in html
    assert '/static/board.js' in html

def test_filter_active():
    cl = c()
    mk(cl, 'still-active-xyz')
    done = mk(cl, 'finished-xyz')
    cl.post(f"/tasks/{done['id']}/complete")
    titles = [t['title'] for t in cl.get('/tasks?status=active').get_json()['tasks']]
    assert 'still-active-xyz' in titles
    assert 'finished-xyz' not in titles

def test_filter_done():
    cl = c()
    d = mk(cl, 'done-only-xyz')
    cl.post(f"/tasks/{d['id']}/complete")
    got = cl.get('/tasks?status=done').get_json()['tasks']
    assert all(t['done'] is True for t in got)
    assert any(t['title'] == 'done-only-xyz' for t in got)

def test_no_filter_returns_all():
    cl = c()
    mk(cl, 'p-xyz')
    b = mk(cl, 'q-xyz')
    cl.post(f"/tasks/{b['id']}/complete")
    titles = [t['title'] for t in cl.get('/tasks').get_json()['tasks']]
    assert 'p-xyz' in titles and 'q-xyz' in titles

def test_delete_removes_task():
    cl = c()
    t = mk(cl, 'temp-del-xyz')
    r = cl.delete(f"/tasks/{t['id']}")
    assert r.status_code == 200
    assert r.get_json() == {'deleted': t['id']}
    remaining = [x['id'] for x in cl.get('/tasks').get_json()['tasks']]
    assert t['id'] not in remaining

def test_delete_unknown_404():
    r = c().delete('/tasks/888888')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}
"#,
        },
        // ===== S3: library authors/books/loans (~8 files — the ceiling probe) =====
        Rung {
            name: "S3-library-loans",
            file_count_hint: 8,
            prompt: "A Flask library API in EIGHT files, app importable as `from app import app`. \
                THREE entities: authors, books, loans. \
                `store.py`: dict-backed stores + low-level fns for authors {\"id\",\"name\"}, books \
                {\"id\",\"title\",\"author_id\"}, and loans {\"id\",\"book_id\",\"returned\":bool}; \
                ids per-entity start at 1. \
                `service.py`: imports store and OWNS the invariants — create_book requires an \
                existing author; delete_author fails if the author has any book; loan_book fails \
                if the book does not exist or is currently on loan (an un-returned loan exists \
                for it). \
                `routes_authors.py`: Blueprint with POST /authors {\"name\"} -> {\"id\",\"name\"} \
                201; DELETE /authors/<int:id> -> {\"ok\":true} 200, {\"error\":\"has books\"} 409 \
                if it has books, {\"error\":\"not found\"} 404 if unknown. \
                `routes_books.py`: Blueprint with POST /books {\"title\",\"author_id\"} -> \
                {\"id\",\"title\",\"author_id\"} 201, or {\"error\":\"no author\"} 400 if the \
                author is missing. \
                `routes_loans.py`: Blueprint with POST /loans {\"book_id\"} -> \
                {\"id\",\"book_id\",\"returned\":false} 201, {\"error\":\"no book\"} 404 if the \
                book is missing, {\"error\":\"on loan\"} 409 if already on loan. \
                `app.py`: creates the app, registers all three blueprints, and GET / -> renders \
                templates/catalog.html (200) listing every book title in a <ul><li>, with \
                <script src=\"/static/catalog.js\"></script>. \
                `templates/catalog.html`: <h1>Catalog</h1>, the <ul> of book titles, the script \
                tag. `static/catalog.js`: plain JS that GETs the catalog data (fetch + DOM). \
                Keep storage in store.py and ALL invariants in service.py. Write plain `def` \
                route handlers, never async def.",
            extend_prompt: "ADD loan returns and availability to the existing library. ALL eight \
                files ALREADY EXIST — read them first (especially service.py for the invariants), \
                then extend. Add POST /loans/<int:id>/return -> {\"id\":id,\"returned\":true} 200, \
                or {\"error\":\"not found\"} 404 for an unknown loan; returning a loan makes its \
                book loanable again (a returned loan no longer blocks loan_book). Add \
                GET /books/<int:id>/availability -> {\"book_id\":id,\"available\":bool} 200 \
                (available == the book exists and has no un-returned loan), or \
                {\"error\":\"no book\"} 404. Keep ALL existing entities, routes, and invariants \
                (no-author, has-books, on-loan) green. Plain def only.",
            oracle_v1: r#"from app import app

def c():
    return app.test_client()

def mk_author(cl, name='Asimov'):
    return cl.post('/authors', json={'name': name}).get_json()['id']

def mk_book(cl, aid, title='Foundation'):
    return cl.post('/books', json={'title': title, 'author_id': aid}).get_json()['id']

def test_create_author_and_book():
    cl = c()
    a = mk_author(cl)
    r = cl.post('/books', json={'title': 'Nightfall', 'author_id': a})
    assert r.status_code == 201
    b = r.get_json()
    assert b['title'] == 'Nightfall' and b['author_id'] == a
    assert isinstance(b['id'], int)

def test_book_requires_author():
    r = c().post('/books', json={'title': 'X', 'author_id': 999999})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'no author'}

def test_delete_author_with_books_409():
    cl = c()
    a = mk_author(cl)
    mk_book(cl, a)
    r = cl.delete(f'/authors/{a}')
    assert r.status_code == 409
    assert r.get_json() == {'error': 'has books'}

def test_delete_empty_author_ok():
    cl = c()
    a = mk_author(cl, 'Lonely')
    r = cl.delete(f'/authors/{a}')
    assert r.status_code == 200
    assert r.get_json() == {'ok': True}

def test_delete_unknown_author_404():
    r = c().delete('/authors/123456')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_loan_book_ok():
    cl = c()
    a = mk_author(cl)
    b = mk_book(cl, a)
    r = cl.post('/loans', json={'book_id': b})
    assert r.status_code == 201
    j = r.get_json()
    assert j['book_id'] == b and j['returned'] is False and isinstance(j['id'], int)

def test_loan_missing_book_404():
    r = c().post('/loans', json={'book_id': 777777})
    assert r.status_code == 404
    assert r.get_json() == {'error': 'no book'}

def test_double_loan_409():
    cl = c()
    a = mk_author(cl)
    b = mk_book(cl, a)
    cl.post('/loans', json={'book_id': b})
    r = cl.post('/loans', json={'book_id': b})
    assert r.status_code == 409
    assert r.get_json() == {'error': 'on loan'}

def test_catalog_page():
    cl = c()
    a = mk_author(cl)
    mk_book(cl, a, title='UniqueTitleZZ')
    html = cl.get('/').get_data(as_text=True)
    assert '<h1>Catalog</h1>' in html
    assert '<li>UniqueTitleZZ</li>' in html
    assert '/static/catalog.js' in html
"#,
            oracle_v2: r#"from app import app

def c():
    return app.test_client()

def mk_author(cl, name='Asimov'):
    return cl.post('/authors', json={'name': name}).get_json()['id']

def mk_book(cl, aid, title='Foundation'):
    return cl.post('/books', json={'title': title, 'author_id': aid}).get_json()['id']

def test_create_author_and_book():
    cl = c()
    a = mk_author(cl)
    r = cl.post('/books', json={'title': 'Nightfall', 'author_id': a})
    assert r.status_code == 201
    b = r.get_json()
    assert b['title'] == 'Nightfall' and b['author_id'] == a
    assert isinstance(b['id'], int)

def test_book_requires_author():
    r = c().post('/books', json={'title': 'X', 'author_id': 999999})
    assert r.status_code == 400
    assert r.get_json() == {'error': 'no author'}

def test_delete_author_with_books_409():
    cl = c()
    a = mk_author(cl)
    mk_book(cl, a)
    r = cl.delete(f'/authors/{a}')
    assert r.status_code == 409
    assert r.get_json() == {'error': 'has books'}

def test_delete_empty_author_ok():
    cl = c()
    a = mk_author(cl, 'Lonely')
    r = cl.delete(f'/authors/{a}')
    assert r.status_code == 200
    assert r.get_json() == {'ok': True}

def test_delete_unknown_author_404():
    r = c().delete('/authors/123456')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_loan_book_ok():
    cl = c()
    a = mk_author(cl)
    b = mk_book(cl, a)
    r = cl.post('/loans', json={'book_id': b})
    assert r.status_code == 201
    j = r.get_json()
    assert j['book_id'] == b and j['returned'] is False and isinstance(j['id'], int)

def test_loan_missing_book_404():
    r = c().post('/loans', json={'book_id': 777777})
    assert r.status_code == 404
    assert r.get_json() == {'error': 'no book'}

def test_double_loan_409():
    cl = c()
    a = mk_author(cl)
    b = mk_book(cl, a)
    cl.post('/loans', json={'book_id': b})
    r = cl.post('/loans', json={'book_id': b})
    assert r.status_code == 409
    assert r.get_json() == {'error': 'on loan'}

def test_catalog_page():
    cl = c()
    a = mk_author(cl)
    mk_book(cl, a, title='UniqueTitleZZ')
    html = cl.get('/').get_data(as_text=True)
    assert '<h1>Catalog</h1>' in html
    assert '<li>UniqueTitleZZ</li>' in html
    assert '/static/catalog.js' in html

def test_return_makes_book_loanable_again():
    cl = c()
    a = mk_author(cl)
    b = mk_book(cl, a)
    loan = cl.post('/loans', json={'book_id': b}).get_json()
    r = cl.post(f"/loans/{loan['id']}/return")
    assert r.status_code == 200
    assert r.get_json() == {'id': loan['id'], 'returned': True}
    again = cl.post('/loans', json={'book_id': b})
    assert again.status_code == 201

def test_return_unknown_loan_404():
    r = c().post('/loans/654321/return')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'not found'}

def test_availability_true_then_false():
    cl = c()
    a = mk_author(cl)
    b = mk_book(cl, a)
    assert cl.get(f'/books/{b}/availability').get_json() == {'book_id': b, 'available': True}
    cl.post('/loans', json={'book_id': b})
    assert cl.get(f'/books/{b}/availability').get_json() == {'book_id': b, 'available': False}

def test_availability_missing_book_404():
    r = c().get('/books/424242/availability')
    assert r.status_code == 404
    assert r.get_json() == {'error': 'no book'}
"#,
        },
    ]
}

#[test]
#[ignore = "live: needs dc-qwen8b backend + docker sandbox"]
fn ab_scale_ladder() {
    let mut cfg = UiConfig::default();
    // Model-swap knob (no code edit needed to benchmark a different backend), mirroring
    // ab_ladder: DC_BASE_URL / DC_MODEL / DC_SUFFIX (set DC_SUFFIX="" to clear /no_think).
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

    let mut rows: Vec<Row> = Vec::new();
    for r in rungs() {
        eprintln!(
            "\n############ SCALE RUNG: {} (~{} files) ############",
            r.name, r.file_count_hint
        );
        let (v1, v2, _files_built) = arm(&cfg, &r);
        let (seq_build, seq_files) = arm_sequential_build(&cfg, &r);
        eprintln!(
            "[{}] WHOLE-TASK v1 green={} {}/{}  |  SEQUENTIAL build green={} {}/{} (files {})  \
             |  v2 green={} {}/{}",
            r.name,
            v1.0, v1.2, v1.3,
            seq_build.0, seq_build.2, seq_build.3, seq_files,
            v2.0, v2.2, v2.3
        );
        rows.push(Row {
            name: r.name.into(),
            file_count_hint: r.file_count_hint,
            v1,
            v2,
            seq_build,
            seq_files,
        });
    }

    eprintln!("\n==================== SCALE LADDER RESULTS ====================");
    eprintln!(
        "{:<20} {:>5} {:>15} {:>15} {:>13}",
        "rung", "files", "WHOLE-TASK", "SEQUENTIAL", "EXTEND (v2)"
    );
    let fmt = |p: &Pass| format!("{} {}/{}", if p.0 { "WIN" } else { "fail" }, p.2, p.3);
    for row in &rows {
        eprintln!(
            "{:<20} {:>5} {:>15} {:>15} {:>13}",
            row.name,
            row.file_count_hint,
            fmt(&row.v1),
            fmt(&row.seq_build),
            fmt(&row.v2)
        );
    }
    // The A/B verdict: where does the sequential per-file BUILD beat the whole-task BUILD?
    eprintln!("\nBUILD A/B (whole-task → sequential):");
    for row in &rows {
        let verdict = match (row.v1.0, row.seq_build.0) {
            (false, true) => "SEQUENTIAL WINS",
            (true, false) => "whole-task wins",
            (true, true) => "tie (both green)",
            (false, false) => "tie (both red)",
        };
        eprintln!(
            "  {:<20} whole={} seq={} (files {})  → {verdict}",
            row.name,
            fmt(&row.v1),
            fmt(&row.seq_build),
            row.seq_files
        );
    }
    eprintln!("=============================================================\n");
    assert_eq!(rows.len(), 3);
}

/// Run one rung: PASS 1 builds greenfield vs oracle_v1, PASS 2 extends the SAME workspace
/// vs oracle_v2 (plan-free). Returns (v1 pass, v2 pass, total source files built).
fn arm(cfg: &UiConfig, rung: &Rung) -> (Pass, Pass, usize) {
    let ws = fresh_ws(cfg, &format!("scale-{}", rung.name));
    let worker = cfg.backend();

    // ---------- PASS 1: greenfield BUILD vs oracle_v1 ----------
    let orchestrator = cfg.orchestrator();
    let plan_ctx = dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        rung.prompt,
        &ws,
        dc_workflow::ThinkPolicy::default(),
        &|_p, _c| {},
    )
    .ok()
    .map(|o| {
        o.state
            .approved()
            .iter()
            .map(|a| format!("=== {} ===\n{}", a.phase.title(), a.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    })
    .unwrap_or_default();

    remove_generated_tests(&ws);
    std::fs::write(ws.join("test_app.py"), rung.oracle_v1).expect("write oracle_v1");

    let plan_block = if plan_ctx.is_empty() {
        String::new()
    } else {
        format!("\n\nPlan:\n{plan_ctx}")
    };
    let build_instr = format!(
        "Implement this project so ALL the existing tests pass: {}\n\n\
         The tests are FROZEN — do not edit any test file. Read test_app.py for the exact \
         contract, then create EVERY source file the task needs (it spans multiple files). \
         Use run_verification; keep editing until green, then finish.{plan_block}",
        rung.prompt
    );
    let v1 = run_pass(cfg, &worker, &ws, &build_instr, BUILD_STEPS);

    // ---------- PASS 2: EXTEND the SAME workspace vs oracle_v2 ----------
    // Do NOT wipe the workspace: the v1 code stays on disk. Swap the frozen contract to the
    // superset and re-run the agent loop PLAN-FREE (the edit-rung pattern) so the model
    // reads the code it built and extends it, rather than re-architecting from scratch.
    remove_generated_tests(&ws);
    std::fs::write(ws.join("test_app.py"), rung.oracle_v2).expect("write oracle_v2");

    let extend_instr = format!(
        "{}\n\n\
         The tests are FROZEN — do not edit any test file. The source files ALREADY EXIST in \
         this workspace — read them first, then modify/extend them. test_app.py now contains \
         BOTH the original tests and new ones; ALL must pass. Use run_verification; keep \
         editing until green, then finish.",
        rung.extend_prompt
    );
    let v2 = run_pass(cfg, &worker, &ws, &extend_instr, EXTEND_STEPS);

    (v1, v2, source_files(&ws).len())
}

/// The SEQUENTIAL per-file BUILD arm (the new path). Plans to get the decomposition board,
/// injects the oracle, then drives the board one file at a time + a final integration pass —
/// instead of dumping the whole task into one loop (which the model batch-discards). Returns
/// just the BUILD pass (v1); EXTEND isn't decomposed (it's read-then-modify), so the A/B is
/// on the build that the whole-task path thrashes.
fn arm_sequential_build(cfg: &UiConfig, rung: &Rung) -> (Pass, usize) {
    let ws = fresh_ws(cfg, &format!("scale-seq-{}", rung.name));
    let orchestrator = cfg.orchestrator();
    let worker = cfg.backend();

    // Plan → board. Then inject the frozen oracle (after planning, before the per-file walk).
    let board = dc_workflow::run_workflow(
        &orchestrator,
        &worker,
        rung.prompt,
        &ws,
        dc_workflow::ThinkPolicy::default(),
        &|_p, _c| {},
    )
    .map(|o| o.board)
    .unwrap_or_default();

    remove_generated_tests(&ws);
    std::fs::write(ws.join("test_app.py"), rung.oracle_v1).expect("write oracle_v1");

    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some("python -m pytest -q 'test_app.py'".to_string());
    agent_cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.max_steps = BUILD_STEPS;
    let sink = dc_core::FnSink(|e: &dc_core::AgentEvent| eprintln!("[S {e:?}]"));

    let report = dc_workflow::build_sequential_with_board(
        board,
        &worker,
        rung.prompt,
        &ws,
        &agent_cfg,
        1, // per-file retry budget
        &sink,
    );
    let (pass, total) = run_and_count(cfg, &ws);
    let green = match &report {
        Ok(r) => r.verified,
        Err(e) => {
            eprintln!("[{} seq] driver failed: {e}", rung.name);
            false
        }
    };
    let steps = report.as_ref().map(|r| r.final_pass.steps).unwrap_or(0);
    ((green, steps, pass, total), source_files(&ws).len())
}

/// Run one agent loop against the frozen test_app.py already on disk, then count pytest.
fn run_pass(
    cfg: &UiConfig,
    worker: &impl ModelBackend,
    ws: &std::path::Path,
    instruction: &str,
    max_steps: usize,
) -> Pass {
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&worker.capabilities());
    let mut agent_cfg = cfg.agent_config(None);
    agent_cfg.verify_command = Some("python -m pytest -q 'test_app.py'".to_string());
    agent_cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    agent_cfg.sandbox = cfg.sandbox();
    agent_cfg.plan_first = false;
    agent_cfg.max_steps = max_steps;
    // Diagnosis hook (thread 2): set DC_DUMP_DIR to capture a readable prompt transcript per
    // run (the exact assembled prompt + raw replies) via the standing TranscriptSink, so the
    // "dump the prompt before assuming a model limit" method is one env var away.
    let eprint = dc_core::FnSink(|e: &dc_core::AgentEvent| eprintln!("[A {e:?}]"));
    let dump = dump_transcript_file(ws);
    if dump.is_some() {
        agent_cfg.verbose = true; // PromptAssembled is gated on verbose
    }
    let report = match &dump {
        Some(file) => {
            let ts = dc_core::TranscriptSink::new(file);
            let tee = dc_core::TeeSink::new(vec![&eprint, &ts]);
            dc_core::run_agent_observed(
                worker, None, &registry, strategy.as_ref(), instruction, ws, &agent_cfg, &tee,
            )
        }
        None => dc_core::run_agent_observed(
            worker, None, &registry, strategy.as_ref(), instruction, ws, &agent_cfg, &eprint,
        ),
    };
    let (pass, total) = run_and_count(cfg, ws);
    match report {
        Ok(r) => (r.verified == Some(true), r.steps, pass, total),
        Err(e) => {
            eprintln!("agent failed: {e}");
            (false, 0, pass, total)
        }
    }
}

/// If `DC_DUMP_DIR` is set, open a per-run transcript file under it (named after the run's
/// workspace dir so each rung/pass gets its own), for the `TranscriptSink`. `None` otherwise.
fn dump_transcript_file(ws: &std::path::Path) -> Option<std::fs::File> {
    let dir = std::env::var("DC_DUMP_DIR").ok()?;
    let _ = std::fs::create_dir_all(&dir);
    let tag = ws
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("run")
        .to_string();
    std::fs::File::create(std::path::Path::new(&dir).join(format!("{tag}.md"))).ok()
}

// ---- helpers (verbatim from ab_ladder.rs) ----

fn run_and_count(cfg: &UiConfig, ws: &std::path::Path) -> (usize, usize) {
    let report =
        dc_verify::run_verification_in(&cfg.sandbox(), ws, "python -m pytest -q 'test_app.py'");
    eprintln!("[verify] {}", report.observation());
    let passed = report.passed_count();
    (passed, passed + report.failed().len())
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
