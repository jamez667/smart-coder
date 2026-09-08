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
            "cargo_info",
            "profile_hotspots",
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
/// Bare names count too, not only backticked ones: `append_file` used to say "write
/// the first part with write_file", and the model reads prose as well as it reads
/// code. Parameter descriptions are checked with the same rule.
#[test]
fn no_tool_description_names_another_tool() {
    let reg = default_registry();
    let names: Vec<&str> = reg.specs().iter().map(|s| s.name).collect();
    for spec in reg.specs() {
        for other in &names {
            if *other == spec.name {
                continue;
            }
            assert!(
                !mentions(spec.description, other),
                "`{}`'s description names {other}, which a trimmed registry may not offer: {:?}",
                spec.name,
                spec.description
            );
            for p in &spec.params {
                assert!(
                    !mentions(p.description, other),
                    "`{}`'s parameter `{}` names {other}, which a trimmed registry may not \
                     offer: {:?}",
                    spec.name,
                    p.name,
                    p.description
                );
            }
        }
    }
}

/// Whether `text` contains `name` as a whole word (not inside a longer identifier).
fn mentions(text: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(i) = text[from..].find(name) {
        let at = from + i;
        let end = at + name.len();
        let before_ok = !text[..at].chars().next_back().is_some_and(is_word);
        let after_ok = !text[end..].chars().next().is_some_and(is_word);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Every default description is one neutral sentence: what the tool does, with no
/// routing ("PREFER", "BEST for", "Use this to"). Routing is the task prefix's job.
#[test]
fn tool_descriptions_are_one_neutral_sentence() {
    for spec in default_registry().specs() {
        let d = spec.description;
        for word in ["PREFER", "BEST", "Use this"] {
            assert!(
                !d.contains(word),
                "`{}` routes with {word:?}: {d:?}",
                spec.name
            );
        }
        // One sentence: ends once, and no sentence break inside. A `.` followed by a
        // space and a capital is the sentence break that matters; dotted paths and
        // `e.g.` are not.
        assert!(
            d.ends_with('.'),
            "`{}` does not end a sentence: {d:?}",
            spec.name
        );
        let breaks = d
            .match_indices(". ")
            .filter(|(i, _)| {
                d[i + 2..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
            })
            .count();
        assert_eq!(
            breaks, 0,
            "`{}` is more than one sentence: {d:?}",
            spec.name
        );
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

/// The build menu is the measured six, in the default registry's order.
///
/// Six tools got `run_command` 12/12 on the SWE-bench path where sixteen got 3/12.
/// The scored task run and the desktop iterate run both offer this; the default
/// registry keeps every tool for the flows that want them.
#[test]
fn the_build_menu_is_the_measured_six() {
    let names: Vec<_> = crate::six_tool_registry()
        .specs()
        .iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "read_file",
            "write_file",
            "edit_file",
            "run_command",
            "run_verification",
            "finish",
        ],
        "the six-tool menu changed; if that is deliberate, probe it and update this test"
    );
    // The build finish stays parameterless here too: the work is the edits on disk.
    let finish = crate::six_tool_registry().get("finish").cloned().unwrap();
    assert!(
        finish.params.is_empty(),
        "the build finish is a signal, not a report"
    );
}

/// The investigation menu is SIX tools, and stays six.
///
/// This is the one model-facing contract in the project with a measurement behind it
/// (six tools: `run_command` 12/12; sixteen: 3/12). `read_only_registry` derives itself
/// from `SideEffect::ReadOnly`, which is the right default for safety but means **any
/// read-only tool added later joins this menu automatically** — silently turning the
/// measured six into a seven nobody probed.
///
/// So the list is pinned here. A tool that belongs in it should be added deliberately,
/// with a probe behind the decision; a tool that does not belongs in `EXCLUDE`.
#[test]
fn the_investigation_menu_is_still_the_measured_six() {
    let names: Vec<_> = crate::read_only_registry()
        .specs()
        .iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "read_file",
            "list_dir",
            "search_code",
            "find_symbol",
            "read_function",
            "finish",
        ],
        "the read-only menu changed; if that is deliberate, probe it and update this test"
    );
}
