//! Decomposition (spec 08 — orchestrator responsibility #1): ask the orchestrator
//! model to break a task into **independent** subtasks, parsed into a
//! [`TaskBoard`].
//!
//! The output contract is a JSON array of objects:
//! `[{"id":"t1","goal":"...","files":["a.py"],"deps":[]}, ...]`. As with the
//! planner, a small model fumbles free-form output, so we parse tolerantly and
//! fall back to a single whole-task subtask rather than failing — decomposition
//! never blocks the swarm (spec 00 — graceful degradation).

use dc_core::extract_json_array;
use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;

use crate::board::{Subtask, TaskBoard};

/// Build the decomposition prompt: the task plus a repo overview, asking for a
/// short list of independent, scoped subtasks as JSON.
fn decompose_messages(task: &str, repo_overview: &str) -> Vec<Message> {
    let system = "You are the orchestrator for a swarm of small coding agents. \
        Break the task into INDEPENDENT subtasks so workers can run in parallel. \
        Make each subtask as SMALL and narrowly-scoped as possible — the smaller the \
        slice, the more reliably a tiny worker completes it — but each subtask owns a \
        DISJOINT set of files: never give the same file to two subtasks; if two \
        pieces of work touch one file, make them ONE subtask (the file is the unit of \
        a worker's edits). Each subtask is a tight, single-purpose goal. Respond with \
        ONLY a JSON array; each item: \
        {\"id\":\"t1\",\"goal\":\"...\",\"files\":[\"path\"],\"deps\":[\"id\"]}. \
        Use deps only when one subtask must finish before another. Keep it minimal."
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
pub fn parse_subtasks(reply: &str) -> Vec<Subtask> {
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
        let files = str_array(item.get("files"));
        let deps = str_array(item.get("deps"));
        out.push(Subtask::new(id, goal).with_files(files).with_deps(deps));
    }
    drop_dangling_deps(&mut out);
    coalesce_by_file(&mut out);
    out
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

/// Ask `orchestrator` to decompose `task` into a [`TaskBoard`]. Always returns a
/// usable board: on an unparseable reply (or backend error) it degrades to a
/// single subtask for the whole task, so the swarm can still run (as a single
/// worker — spec 08's degenerate case).
pub fn decompose(
    orchestrator: &dyn ModelBackend,
    task: &str,
    repo_overview: &str,
) -> Result<TaskBoard> {
    let req = GenerateRequest::new(decompose_messages(task, repo_overview));
    let subtasks = match orchestrator.generate(&req) {
        Ok(resp) => parse_subtasks(&resp.content),
        Err(_) => Vec::new(),
    };
    let board = if subtasks.is_empty() {
        TaskBoard::new(vec![Subtask::new("t1", task)])
    } else {
        TaskBoard::new(subtasks)
    };
    Ok(board)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::MockBackend;

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
