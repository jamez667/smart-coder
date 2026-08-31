//! The registry surface, and the dispatch smoke tests.

use serde_json::json;

use super::{call, temp_dir};
use crate::builtin::dispatch::{execute, ToolOutcome};
use crate::builtin::registry::default_registry;

#[test]
fn default_registry_has_the_v1_tools() {
    let names: Vec<_> = default_registry().specs().iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec![
            "read_file",
            "list_dir",
            "search_code",
            "find_symbol",
            "write_file",
            "create_file",
            "append_file",
            "edit_file",
            "edit_lines",
            "read_function",
            "edit_function",
            "run_command",
            "run_verification",
            "update_plan",
            "ask_user",
            "finish"
        ]
    );
}

#[test]
fn write_then_read_roundtrips() {
    let ws = temp_dir("rw");
    let w = call(json!({"tool":"write_file","path":"sub/f.txt","content":"hello"}));
    assert!(matches!(execute(&w, &ws), ToolOutcome::Observation(_)));

    let r = call(json!({"tool":"read_file","path":"sub/f.txt"}));
    match execute(&r, &ws) {
        ToolOutcome::Observation(o) => assert!(o.contains("hello"), "got: {o}"),
        _ => panic!("expected observation"),
    }
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn finish_is_finished() {
    let ws = temp_dir("fin");
    assert!(matches!(
        execute(&call(json!({"tool":"finish"})), &ws),
        ToolOutcome::Finished
    ));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn rejects_path_traversal() {
    let ws = temp_dir("trav");
    match execute(&call(json!({"tool":"read_file","path":"../secret"})), &ws) {
        ToolOutcome::Observation(o) => assert!(o.contains("rejected"), "got: {o}"),
        _ => panic!(),
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// No tool description may name ANOTHER tool.
///
/// The registry gets trimmed -- a scored task run offers six of these, not all
/// sixteen -- so a description that points at a sibling tool is steering the
/// model toward something it may have no way to call. `read_file` used to say
/// "after `search_code` gives you a line number", and a trimmed run has no
/// `search_code`; the model then guessed at parameters and lost the turn
/// ("tool read_file has no parameter end").
///
/// A description may name its OWN parameters; those always exist.
#[test]
fn no_tool_description_names_another_tool() {
    let reg = default_registry();
    let names: Vec<&str> = reg.specs().iter().map(|s| s.name).collect();
    for spec in reg.specs() {
        let own: Vec<String> = spec
            .params
            .iter()
            .map(|p| format!("`{}`", p.name))
            .collect();
        for other in &names {
            if *other == spec.name {
                continue;
            }
            let token = format!("`{other}`");
            if own.contains(&token) {
                continue;
            }
            assert!(
                !spec.description.contains(&token),
                "`{}`'s description names `{other}`, which a trimmed registry may not offer",
                spec.name
            );
        }
    }
}
