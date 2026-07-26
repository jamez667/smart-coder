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
