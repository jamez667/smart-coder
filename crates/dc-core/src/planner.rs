//! The planner (spec 03 — PLAN): ask the model for a short ordered step list,
//! grounded in a retrieved repo overview, and parse it into a harness-owned
//! [`PlanState`].
//!
//! Planning is one model call with a tightly-scoped output contract (a JSON array
//! of short step strings). A small model fumbles free-form output, so we parse
//! tolerantly and fall back rather than crash: a bad plan becomes a trivial
//! single-step plan ("complete the task"), so the agent can still proceed and
//! re-plan later — planning never blocks the loop.

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;

use crate::plan::PlanState;
use crate::text::extract_json_array;

/// Build the planner prompt: the task plus a repo overview, asking for a short
/// ordered list of concrete steps as a JSON array of strings.
fn planner_messages(task: &str, repo_overview: &str) -> Vec<Message> {
    let system = "You are the planner for a small coding agent. Break the task into \
        a SHORT ordered list of 2-6 concrete, single-action steps (e.g. \"locate \
        where X is defined\", \"edit function Y\", \"run the tests\"). Respond with \
        ONLY a JSON array of step strings, nothing else. Keep each step tiny."
        .to_string();
    let mut user = format!("Task: {task}");
    if !repo_overview.is_empty() {
        user.push_str("\n\nRepo overview:\n");
        user.push_str(repo_overview);
    }
    user.push_str("\n\nReturn the plan as a JSON array of step strings.");
    vec![Message::system(system), Message::user(user)]
}

/// Parse a planner reply (a JSON array of strings, tolerating surrounding prose)
/// into step descriptions. Returns an empty vec if nothing parseable is found.
pub fn parse_plan(reply: &str) -> Vec<String> {
    let Some(arr) = extract_json_array(reply) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arr) else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Ask `backend` to plan `task`, grounded in `repo_overview`. Always returns a
/// usable plan: on an unparseable reply (or a backend error) it falls back to a
/// single generic step so the loop can still run and re-plan later.
pub fn make_plan(backend: &dyn ModelBackend, task: &str, repo_overview: &str) -> Result<PlanState> {
    let req = GenerateRequest::new(planner_messages(task, repo_overview));
    // A backend error here is *not* fatal to planning — degrade to a trivial plan
    // (spec 00 — graceful degradation). Only propagate if the caller cares; we
    // choose resilience so the agent can always start.
    let descriptions = match backend.generate(&req) {
        Ok(resp) => parse_plan(&resp.content),
        Err(_) => Vec::new(),
    };
    let plan = if descriptions.is_empty() {
        PlanState::from_descriptions(["Complete the task and make the tests pass"])
    } else {
        PlanState::from_descriptions(descriptions)
    };
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::MockBackend;

    #[test]
    fn parses_a_clean_json_array() {
        let steps = parse_plan(r#"["locate foo", "edit foo", "run tests"]"#);
        assert_eq!(steps, vec!["locate foo", "edit foo", "run tests"]);
    }

    #[test]
    fn tolerates_prose_around_the_array() {
        let reply = "Sure, here's the plan:\n[\"step one\", \"step two\"]\nGood luck!";
        assert_eq!(parse_plan(reply), vec!["step one", "step two"]);
    }

    #[test]
    fn drops_empty_and_non_string_items() {
        let steps = parse_plan(r#"["a", "", 42, "  ", "b"]"#);
        assert_eq!(steps, vec!["a", "b"]);
    }

    #[test]
    fn unparseable_reply_yields_no_steps() {
        assert!(parse_plan("I will start by reading the file.").is_empty());
        assert!(parse_plan("").is_empty());
    }

    #[test]
    fn make_plan_uses_the_models_steps() {
        let backend = MockBackend::new([r#"["find the bug", "fix it", "verify"]"#]);
        let plan = make_plan(&backend, "fix the bug", "").unwrap();
        assert_eq!(plan.steps().len(), 3);
        assert_eq!(plan.current_step(), Some("find the bug"));
    }

    #[test]
    fn make_plan_falls_back_when_the_model_returns_garbage() {
        let backend = MockBackend::new(["no json here, sorry"]);
        let plan = make_plan(&backend, "do the thing", "").unwrap();
        // A usable single-step plan, not an empty/blocked one.
        assert_eq!(plan.steps().len(), 1);
        assert!(!plan.is_empty());
    }

    #[test]
    fn make_plan_falls_back_on_backend_error() {
        // An exhausted mock errors on generate -> graceful degradation to a plan.
        let backend = MockBackend::new(Vec::<String>::new());
        let plan = make_plan(&backend, "task", "").unwrap();
        assert_eq!(plan.steps().len(), 1);
    }

    /// The planner prompt should carry the repo overview when present.
    #[test]
    fn planner_prompt_includes_repo_overview() {
        let msgs = planner_messages("task", "repo map: foo.rs:1 main");
        let user = &msgs[1].content;
        assert!(user.contains("repo map: foo.rs:1 main"), "{user}");
    }
}
