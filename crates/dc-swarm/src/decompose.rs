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
        Break the task into 2-5 INDEPENDENT subtasks that touch as few shared \
        files as possible, so workers can run in parallel. Each subtask is a tight, \
        single-purpose goal. Respond with ONLY a JSON array; each item: \
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
    out
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
}
