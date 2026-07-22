//! Decomposition (spec 08 — orchestrator responsibility #1): ask the orchestrator
//! model to break a task into **independent** subtasks, parsed into a
//! [`TaskBoard`].
//!
//! The output contract is a JSON array of objects:
//! `[{"id":"t1","goal":"...","files":["a.py"],"deps":[]}, ...]`. As with the
//! planner, a small model fumbles free-form output, so we parse tolerantly and
//! fall back to a single whole-task subtask rather than failing — decomposition
//! never blocks the swarm (spec 00 — graceful degradation).

use sc_core::extract_json_array;
use sc_model::{GenerateRequest, Message, ModelBackend};
use sc_proto::Result;

use crate::board::{Subtask, TaskBoard};

/// Build the decomposition prompt: the task plus a repo overview, asking for a
/// short list of independent, scoped subtasks as JSON.
fn decompose_messages(task: &str, repo_overview: &str) -> Vec<Message> {
    // Keep this SHORT. A long, rationale-heavy system prompt makes the small
    // orchestrator emit *nothing* (observed live 2026-06-14: coder-0 returned empty
    // content for the verbose version, with and without /no_think, so every run fell
    // back to a single file-less subtask). A concise instruction with one concrete
    // example reliably yields a JSON array with a populated `files` field — which is
    // what `integrate` needs to have a target to merge into. The disjoint-files
    // invariant is enforced structurally by `coalesce_by_file`, so it needn't be
    // belaboured in the prompt.
    // The trailing `/no_think` is load-bearing on the Qwen3-class orchestrator: WITHOUT
    // it the model frequently spends its whole budget in a reasoning block and returns
    // EMPTY content, so the swarm falls back to a single trivial subtask (observed live
    // 2026-06-14). An earlier note here claimed `/no_think` didn't help — it does once
    // the prompt is also kept short, which it now is. Suppressing the think block yields
    // a populated JSON array reliably.
    let system = "Break the coding task into independent subtasks, one JSON object per \
        subtask. Fields: id, goal, files (the real source files this subtask edits), \
        deps (ids that must finish first). Put work that touches the same file in ONE \
        subtask. The test files are fixed — never create a subtask that edits a test \
        file; subtasks only change the source code that must make the existing tests \
        pass. Output ONLY a JSON array, e.g. \
        [{\"id\":\"t1\",\"goal\":\"fix the parser\",\"files\":[\"parser.py\"],\"deps\":[]}]. \
        /no_think"
        .to_string();
    let mut user = format!("Task: {task}");
    if !repo_overview.is_empty() {
        user.push_str("\n\nRepo overview:\n");
        user.push_str(repo_overview);
    }
    user.push_str("\n\nReturn the subtasks as a JSON array.");
    vec![Message::system(system), Message::user(user)]
}

/// Parse a decomposition reply (a JSON array of subtask objects, tolerating
/// surrounding prose) into subtasks. Returns empty if nothing parseable.
/// Parse the decomposition reply into subtasks. `on_stack_exts` is the set of file extensions
/// that belong to THIS project's stack (e.g. `["rs"]` for a cargo repo, `["py","js","html","css"]`
/// for the Python eval ladder); a subtask whose files are ALL off-stack is dropped as language
/// drift. Pass an empty slice to disable the filter (keep every file). Use [`parse_subtasks`] for
/// the historical Python-ladder defaults.
pub fn parse_subtasks_on_stack(reply: &str, on_stack_exts: &[&str]) -> Vec<Subtask> {
    let Some(arr) = extract_json_array(reply) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arr) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let goal = item
            .get("goal")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if goal.is_empty() {
            continue;
        }
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("t{}", i + 1));
        let raw_files = str_array(item.get("files"));
        // Stack lock: drop files in off-stack languages (a stray `.ts`/`.go`/`.java`).
        // A subtask that named files but ends up with none was purely off-stack work
        // (e.g. a Node.js/TypeScript module when the backend must be Python) — skip it
        // so a drifted language can't derail the build. See `is_on_stack`.
        let had_files = !raw_files.is_empty();
        let files: Vec<String> = raw_files
            .into_iter()
            .filter(|f| is_on_stack(f, on_stack_exts))
            .collect();
        if had_files && files.is_empty() {
            continue;
        }
        let deps = str_array(item.get("deps"));
        out.push(Subtask::new(id, goal).with_files(files).with_deps(deps));
    }
    drop_dangling_deps(&mut out);
    coalesce_by_file(&mut out);
    out
}

/// The historical entry point: parse with the Python eval-ladder's on-stack set
/// (`py`/`js`/`html`/`css` + data/config). Kept so existing callers/tests are unchanged; the
/// stack-aware [`parse_subtasks_on_stack`] is what a real Rust/other-language project uses.
pub fn parse_subtasks(reply: &str) -> Vec<Subtask> {
    parse_subtasks_on_stack(reply, PY_LADDER_EXTS)
}

/// On-stack extensions for the Python eval ladder (the historical default).
const PY_LADDER_EXTS: &[&str] = &["py", "js", "html", "css"];

/// Whether a file belongs on THIS project's stack (per `on_stack_exts`), so language DRIFT can
/// be dropped without discarding the project's own files. The bug this fixes: the extensions were
/// hardcoded to the Python ladder, so `.rs`/`.cs`/`.go` were treated as off-stack — silently
/// dropping EVERY subtask on a Rust/other-language project and leaving an empty board (the staged
/// pipeline then "built" nothing).
///
/// Rules: an empty `on_stack_exts` disables the filter (keep everything). A file whose extension
/// is in the set is on-stack. A file whose extension is a KNOWN CODE language NOT in the set is
/// drift → off-stack. Non-code data/config files (`.txt`/`.json`/`.sql`/templates/no extension)
/// are stack-neutral → kept. The Python-ladder special case — Node backend `.js` under a
/// `server/`/`backend/` path — is still rejected when `js` is on-stack (the backend must be
/// Python), preserving the original anti-Node-drift behavior.
fn is_on_stack(path: &str, on_stack_exts: &[&str]) -> bool {
    if on_stack_exts.is_empty() {
        return true; // filter disabled
    }
    // Known code extensions (any language). A file with one of these that is NOT on this stack is
    // drift; a file with a non-code extension (or none) is neutral and kept.
    const KNOWN_CODE: &[&str] = &[
        "rs", "py", "js", "ts", "tsx", "jsx", "java", "go", "rb", "php", "cs", "cpp", "cc", "c",
        "h", "hpp", "kt", "swift", "scala", "ex", "exs", "dart", "m", "mm",
    ];
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().filter(|e| *e != lower); // None if no '.'
    let on_stack = |e: &str| on_stack_exts.iter().any(|x| x.eq_ignore_ascii_case(e));

    if let Some(ext) = ext {
        // A code file in a language that's NOT this project's stack → drift.
        if KNOWN_CODE.contains(&ext) && !on_stack(ext) {
            return false;
        }
    }
    // Python-ladder anti-Node-drift: a `.js` under a backend path is Node — reject only when JS is
    // an on-stack frontend language (the backend must be Python).
    if lower.ends_with(".js")
        && on_stack("js")
        && (lower.starts_with("server/")
            || lower.starts_with("backend/")
            || lower.contains("/server/")
            || lower.contains("/backend/"))
    {
        return false;
    }
    true
}

/// Enforce **one file → one worker** (the unit of serialized writes is the file).
///
/// Two workers editing the same file independently each propose a whole-file
/// snapshot; integrating either reverts the other's work, so both get rejected.
/// We don't trust the small orchestrator to honour "touch as few shared files as
/// possible", so the harness enforces it: any subtasks that share a file are
/// merged into one subtask (its goal a union, its files/deps unioned). The merged
/// subtask keeps the earliest id so deps pointing at any merged member still
/// resolve.
fn coalesce_by_file(subtasks: &mut Vec<Subtask>) {
    use std::collections::HashMap;

    // Union-find over subtask indices keyed by shared file.
    let mut parent: Vec<usize> = (0..subtasks.len()).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while parent[r] != r {
            r = parent[r];
        }
        // Path-compress.
        let mut c = i;
        while parent[c] != r {
            let next = parent[c];
            parent[c] = r;
            c = next;
        }
        r
    }

    let mut owner: HashMap<String, usize> = HashMap::new();
    // Collect (file, index) pairs first to avoid borrowing `subtasks` mutably and
    // immutably at once.
    let file_idx: Vec<(String, usize)> = subtasks
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.files.iter().map(move |f| (f.clone(), i)))
        .collect();
    for (file, i) in file_idx {
        match owner.get(&file) {
            Some(&j) => {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
            None => {
                owner.insert(file, i);
            }
        }
    }

    // No shared files ⇒ nothing to merge.
    if (0..subtasks.len()).all(|i| find(&mut parent, i) == i) {
        return;
    }

    // Group members by their representative root, preserving original order.
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for i in 0..subtasks.len() {
        let root = find(&mut parent, i);
        match groups.iter_mut().find(|(r, _)| *r == root) {
            Some((_, members)) => members.push(i),
            None => groups.push((root, vec![i])),
        }
    }

    // Build merged subtasks. The merged id is the first member's id; record the
    // map from every member id → merged id so deps can be rewritten.
    let mut id_remap: HashMap<String, String> = HashMap::new();
    let mut merged: Vec<Subtask> = Vec::with_capacity(groups.len());
    for (_, members) in &groups {
        let first = &subtasks[members[0]];
        let merged_id = first.id.clone();

        if members.len() == 1 {
            merged.push(first.clone());
            id_remap.insert(merged_id.clone(), merged_id);
            continue;
        }

        let mut goals: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut deps: Vec<String> = Vec::new();
        for &m in members {
            let s = &subtasks[m];
            id_remap.insert(s.id.clone(), merged_id.clone());
            goals.push(s.goal.clone());
            for f in &s.files {
                if !files.contains(f) {
                    files.push(f.clone());
                }
            }
            for d in &s.deps {
                if !deps.contains(d) {
                    deps.push(d.clone());
                }
            }
        }
        merged.push(
            Subtask::new(merged_id, goals.join("; "))
                .with_files(files)
                .with_deps(deps),
        );
    }

    // Rewrite deps through the remap (a dep on a merged member now points at the
    // merged id), then drop self/dangling deps the merge may have created.
    for s in merged.iter_mut() {
        let mut new_deps: Vec<String> = Vec::new();
        for d in &s.deps {
            let mapped = id_remap.get(d).cloned().unwrap_or_else(|| d.clone());
            if mapped != s.id && !new_deps.contains(&mapped) {
                new_deps.push(mapped);
            }
        }
        s.deps = new_deps;
    }
    drop_dangling_deps(&mut merged);

    *subtasks = merged;
}

/// Remove deps that point at ids not present (a small model may invent them),
/// so the DAG can't deadlock on a phantom dependency.
fn drop_dangling_deps(subtasks: &mut [Subtask]) {
    let ids: std::collections::HashSet<String> = subtasks.iter().map(|s| s.id.clone()).collect();
    for s in subtasks.iter_mut() {
        s.deps.retain(|d| ids.contains(d) && d != &s.id);
    }
}

fn str_array(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A decomposition plus the exact prompt/reply that produced it, so the harness can
/// surface *what the orchestrator was asked and answered* (the swarm UI renders this).
#[derive(Debug, Clone)]
pub struct Decomposition {
    pub board: TaskBoard,
    /// The full prompt sent to the orchestrator (system + user, joined for display).
    pub prompt: String,
    /// The orchestrator's raw reply, or a `(…)` note when it errored/returned empty.
    pub reply: String,
    /// Whether the board is the trivial single-subtask fallback (the model gave us
    /// nothing parseable even after a retry).
    pub fell_back: bool,
}

/// How many times to re-ask the orchestrator when it returns an empty/unparseable
/// reply before falling back to the trivial single-subtask board. A small model
/// occasionally returns an empty completion; one retry recovers most of those without
/// silently collapsing the task into one worker.
const DECOMPOSE_RETRIES: usize = 2;

/// Ask `orchestrator` to decompose `task`, returning the board **and** the prompt/reply
/// (spec 08). Retries on an empty/unparseable reply before degrading to a single
/// whole-task subtask, so a flaky empty completion doesn't silently collapse the work.
pub fn decompose_observed(
    orchestrator: &dyn ModelBackend,
    task: &str,
    repo_overview: &str,
) -> Decomposition {
    let messages = decompose_messages(task, repo_overview);
    let prompt = messages
        .iter()
        .map(|m| format!("[{:?}] {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut req = GenerateRequest::new(messages);
    // A real decomposition of a multi-part app is several subtasks of JSON — well over
    // the 1024-token default, which truncates the array mid-object so it won't parse and
    // the whole (good) decomposition is discarded as a fallback (observed live
    // 2026-06-14: "a websocket powered chat website" produced a perfect 5-subtask plan
    // that was cut off at t5 and thrown away). Give the one-shot decomposer room.
    req.max_tokens = 4096;

    let mut last_reply = String::new();
    for _ in 0..=DECOMPOSE_RETRIES {
        match orchestrator.generate(&req) {
            Ok(resp) => {
                last_reply = resp.content.clone();
                let subtasks = parse_subtasks(&resp.content);
                if !subtasks.is_empty() {
                    return Decomposition {
                        board: TaskBoard::new(subtasks),
                        prompt,
                        reply: last_reply,
                        fell_back: false,
                    };
                }
                // Empty/unparseable — retry.
            }
            Err(e) => last_reply = format!("(backend error: {e})"),
        }
    }

    // Every attempt came back empty/unparseable: degrade to one whole-task subtask so
    // the swarm still runs (spec 08's degenerate case), but flag it so the UI can warn.
    Decomposition {
        board: TaskBoard::new(vec![Subtask::new("t1", task)]),
        prompt,
        reply: if last_reply.is_empty() {
            "(orchestrator returned empty after retries)".to_string()
        } else {
            last_reply
        },
        fell_back: true,
    }
}

/// Ask `orchestrator` to decompose `task` into a [`TaskBoard`]. Always returns a
/// usable board: on an unparseable reply (or backend error) it degrades to a
/// single subtask for the whole task, so the swarm can still run (as a single
/// worker — spec 08's degenerate case). Thin wrapper over [`decompose_observed`].
pub fn decompose(
    orchestrator: &dyn ModelBackend,
    task: &str,
    repo_overview: &str,
) -> Result<TaskBoard> {
    Ok(decompose_observed(orchestrator, task, repo_overview).board)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::MockBackend;

    #[test]
    fn rust_subtask_survives_on_a_rust_stack() {
        // The root-cause regression: a `.rs` subtask was dropped because `.rs` was hardcoded as
        // off-stack, leaving an empty board so the staged pipeline built nothing for a Rust repo.
        let reply = r#"[{"id":"t1","goal":"add Gunner variant","files":["crates/void_sim/src/ship_template/schema.rs"],"deps":[]}]"#;
        let subs = parse_subtasks_on_stack(reply, &["rs"]);
        assert_eq!(subs.len(), 1, "the .rs subtask must survive: {subs:?}");
        assert_eq!(
            subs[0].files,
            vec!["crates/void_sim/src/ship_template/schema.rs"]
        );
        // Sanity: the OLD Python-default would have dropped it.
        assert!(
            parse_subtasks(reply).is_empty(),
            "python default still drops .rs (drift)"
        );
    }

    #[test]
    fn language_drift_is_still_dropped() {
        // On a Rust stack, a stray Python/TS subtask is drift → dropped.
        let reply = r#"[{"id":"t1","goal":"stray python","files":["helper.py"],"deps":[]},
                        {"id":"t2","goal":"real rust","files":["src/lib.rs"],"deps":[]}]"#;
        let subs = parse_subtasks_on_stack(reply, &["rs"]);
        assert_eq!(subs.len(), 1, "only the .rs task survives: {subs:?}");
        assert_eq!(subs[0].id, "t2");
    }

    #[test]
    fn empty_exts_keeps_everything() {
        // Unknown stack → filter disabled → nothing dropped (better an unfiltered board than none).
        let reply = r#"[{"id":"t1","goal":"go file","files":["main.go"],"deps":[]}]"#;
        assert_eq!(parse_subtasks_on_stack(reply, &[]).len(), 1);
    }

    #[test]
    fn non_code_files_are_stack_neutral() {
        // A data/config/template file isn't a language, so it's kept on any stack.
        let reply =
            r#"[{"id":"t1","goal":"config","files":["config.json","src/lib.rs"],"deps":[]}]"#;
        let subs = parse_subtasks_on_stack(reply, &["rs"]);
        assert_eq!(subs.len(), 1);
        assert!(
            subs[0].files.contains(&"config.json".to_string()),
            "{:?}",
            subs[0].files
        );
    }

    #[test]
    fn parses_a_clean_subtask_array() {
        let reply = r#"[
            {"id":"a","goal":"add validation","files":["parse.py"],"deps":[]},
            {"id":"b","goal":"add a test","files":["test_parse.py"],"deps":["a"]}
        ]"#;
        let subs = parse_subtasks(reply);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].id, "a");
        assert_eq!(subs[0].files, vec!["parse.py"]);
        assert_eq!(subs[1].deps, vec!["a"]);
    }

    #[test]
    fn tolerates_prose_and_fills_missing_ids() {
        let reply = "Here's the plan:\n[{\"goal\":\"fix bug\"},{\"goal\":\"add test\"}]\nGo!";
        let subs = parse_subtasks(reply);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].id, "t1");
        assert_eq!(subs[1].id, "t2");
    }

    #[test]
    fn rejects_node_react_keeps_python_and_plain_frontend() {
        // Stack lock: Python backend + plain JS/HTML/CSS frontend. A Node-backend .js,
        // a React .jsx, and any off-stack language are dropped; Python, plain frontend
        // JS/HTML, and data files survive.
        let reply = r#"[
            {"id":"a","goal":"py route","files":["server/app.py"]},
            {"id":"b","goal":"node backend","files":["server/src/db.js"]},
            {"id":"c","goal":"react ui","files":["client/HomePage.jsx"]},
            {"id":"d","goal":"go thing","files":["main.go"]},
            {"id":"e","goal":"frontend js","files":["static/app.js","index.html"]},
            {"id":"f","goal":"schema","files":["schema.sql"]}
        ]"#;
        let subs = parse_subtasks(reply);
        let ids: Vec<&str> = subs.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"a"), "python backend kept");
        assert!(!ids.contains(&"b"), "Node-backend .js (server/) dropped");
        assert!(!ids.contains(&"c"), "React .jsx dropped");
        assert!(!ids.contains(&"d"), "off-stack .go dropped");
        assert!(ids.contains(&"e"), "plain frontend .js + .html kept");
        assert!(ids.contains(&"f"), "data file .sql kept");
    }

    #[test]
    fn drops_dangling_and_self_deps() {
        let reply = r#"[{"id":"a","goal":"x","deps":["ghost","a"]}]"#;
        let subs = parse_subtasks(reply);
        assert!(subs[0].deps.is_empty(), "dangling/self deps removed");
    }

    #[test]
    fn skips_items_without_a_goal() {
        let subs = parse_subtasks(r#"[{"id":"a"},{"id":"b","goal":"real"}]"#);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].goal, "real");
    }

    #[test]
    fn decompose_uses_the_models_subtasks() {
        let backend = MockBackend::new([r#"[{"id":"a","goal":"do a"},{"id":"b","goal":"do b"}]"#]);
        let board = decompose(&backend, "the task", "").unwrap();
        assert_eq!(board.len(), 2);
    }

    #[test]
    fn decompose_falls_back_to_one_subtask_on_garbage() {
        let backend = MockBackend::new(["no json here"]);
        let board = decompose(&backend, "the whole task", "").unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board.subtasks()[0].goal, "the whole task");
    }

    #[test]
    fn decompose_falls_back_on_backend_error() {
        let backend = MockBackend::new(Vec::<String>::new());
        let board = decompose(&backend, "task", "").unwrap();
        assert_eq!(board.len(), 1);
    }

    #[test]
    fn decompose_retries_past_an_empty_reply() {
        // A flaky orchestrator returns empty first, then a real array on retry. The
        // swarm must NOT collapse to one trivial subtask on the first empty.
        let backend =
            MockBackend::new(["", r#"[{"id":"a","goal":"do a"},{"id":"b","goal":"do b"}]"#]);
        let d = decompose_observed(&backend, "the task", "");
        assert_eq!(d.board.len(), 2, "the retry's good reply is used");
        assert!(!d.fell_back);
    }

    #[test]
    fn decompose_flags_fell_back_after_all_empties() {
        // Every attempt empty ⇒ the trivial fallback, flagged so the UI can warn.
        let backend = MockBackend::new(["", "", ""]);
        let d = decompose_observed(&backend, "the whole task", "");
        assert_eq!(d.board.len(), 1);
        assert_eq!(d.board.subtasks()[0].goal, "the whole task");
        assert!(
            d.fell_back,
            "fell_back is set when nothing parseable came back"
        );
    }

    #[test]
    fn decompose_surfaces_the_prompt_and_reply() {
        let backend = MockBackend::new([r#"[{"id":"a","goal":"do a"}]"#]);
        let d = decompose_observed(&backend, "the task", "");
        assert!(d.prompt.contains("the task"), "prompt carries the task");
        assert!(d.reply.contains("do a"), "reply is the model's raw output");
    }

    #[test]
    fn coalesces_subtasks_that_share_a_file() {
        // Two subtasks both touch mathlib.py — they MUST merge into one, else two
        // workers race on whole-file snapshots and both get rejected at integration.
        let reply = r#"[
            {"id":"a","goal":"fix is_even","files":["mathlib.py"],"deps":[]},
            {"id":"b","goal":"fix double","files":["mathlib.py"],"deps":[]}
        ]"#;
        let subs = parse_subtasks(reply);
        assert_eq!(subs.len(), 1, "shared-file subtasks merge: {subs:?}");
        assert_eq!(subs[0].files, vec!["mathlib.py"]);
        // The merged goal carries both pieces of work.
        assert!(subs[0].goal.contains("is_even"));
        assert!(subs[0].goal.contains("double"));
    }

    #[test]
    fn keeps_subtasks_on_distinct_files_separate() {
        let reply = r#"[
            {"id":"a","goal":"edit a.py","files":["a.py"]},
            {"id":"b","goal":"edit b.py","files":["b.py"]}
        ]"#;
        let subs = parse_subtasks(reply);
        assert_eq!(subs.len(), 2, "distinct files stay parallel");
    }

    #[test]
    fn merge_is_transitive_across_a_shared_file() {
        // a&b share x.py; b&c share y.py ⇒ all three collapse into one.
        let reply = r#"[
            {"id":"a","goal":"A","files":["x.py"]},
            {"id":"b","goal":"B","files":["x.py","y.py"]},
            {"id":"c","goal":"C","files":["y.py"]}
        ]"#;
        let subs = parse_subtasks(reply);
        assert_eq!(
            subs.len(),
            1,
            "transitive file sharing merges all: {subs:?}"
        );
        let mut files = subs[0].files.clone();
        files.sort();
        assert_eq!(files, vec!["x.py", "y.py"]);
    }

    #[test]
    fn merge_rewrites_deps_to_the_merged_id() {
        // c depends on b; a&b merge (shared file) ⇒ c's dep retargets to the merged
        // id "a" and is NOT dropped as dangling.
        let reply = r#"[
            {"id":"a","goal":"A","files":["shared.py"]},
            {"id":"b","goal":"B","files":["shared.py"]},
            {"id":"c","goal":"C","files":["c.py"],"deps":["b"]}
        ]"#;
        let subs = parse_subtasks(reply);
        assert_eq!(subs.len(), 2);
        let c = subs.iter().find(|s| s.id == "c").unwrap();
        assert_eq!(c.deps, vec!["a"], "dep on merged member retargets: {c:?}");
    }

    #[test]
    fn subtasks_without_files_are_never_merged() {
        // No file info ⇒ we can't prove a conflict ⇒ leave them parallel.
        let reply = r#"[{"id":"a","goal":"A"},{"id":"b","goal":"B"}]"#;
        let subs = parse_subtasks(reply);
        assert_eq!(subs.len(), 2);
    }
}
