//! The registry surface, and the dispatch smoke tests.

use serde_json::json;

use super::{call, temp_dir};
use crate::builtin::dispatch::{execute, handled_here, ToolOutcome, NOT_EXECUTED_HERE};
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

/// **Every registry tool is either executed here or named as one that is not.**
///
/// The registry declares `run_command` and `run_verification`, but this crate's
/// executor cannot run them -- they spawn processes and need run configuration
/// (the sandbox, the verify command, the confirm gate) that `sc-tools` deliberately
/// does not know about, so `sc-core` owns them. Calling one here returns an
/// `internal: no executor` observation rather than a compile error, which is a real
/// hazard for any caller that is not the agent loop.
///
/// This pins the split from both ends: a new process-spawning tool added to the
/// registry without being listed in `NOT_EXECUTED_HERE` fails here, rather than
/// silently returning a plausible-looking observation at run time.
#[test]
fn every_registry_tool_is_executable_here_or_declared_otherwise() {
    let ws = temp_dir("exec-split");
    for spec in default_registry().specs() {
        if !handled_here(spec.name) {
            // Declared as sc-core's: it must really be one of the process tools.
            assert!(
                NOT_EXECUTED_HERE.contains(&spec.name),
                "{} is not handled here but is not in NOT_EXECUTED_HERE",
                spec.name
            );
            continue;
        }
        // Everything else must have an executor arm. A missing one shows up as the
        // "no executor" fallthrough; `finish` is the one non-fs outcome.
        // Validation needs the required args present, so build a minimal call from
        // the spec itself: the point is which EXECUTOR arm runs, not the arguments.
        let mut v = serde_json::Map::new();
        v.insert("tool".into(), json!(spec.name));
        for p in &spec.params {
            let filler = match p.ty {
                crate::spec::ParamType::Integer | crate::spec::ParamType::OptionalInteger => {
                    json!(1)
                }
                _ => json!("x"),
            };
            v.insert(p.name.to_string(), filler);
        }
        let Ok(validated) = default_registry().validate(&serde_json::Value::Object(v)) else {
            continue; // a spec this filler cannot satisfy is not what we are testing
        };
        let outcome = execute(&validated, &ws);
        if let ToolOutcome::Observation(o) = &outcome {
            assert!(
                !o.starts_with("internal: no executor"),
                "{} has no executor arm in sc-tools, and is not declared as a \
                 process tool -- a caller would get a plausible-looking observation \
                 instead of a result",
                spec.name
            );
        }
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// The names in `NOT_EXECUTED_HERE` must actually exist in the registry, or the list is
/// stale and the guard above passes vacuously.
#[test]
fn process_tools_are_real_registry_tools() {
    let reg = default_registry();
    for name in NOT_EXECUTED_HERE {
        assert!(
            reg.get(name).is_some(),
            "NOT_EXECUTED_HERE names {name}, which the registry does not declare"
        );
    }
}
