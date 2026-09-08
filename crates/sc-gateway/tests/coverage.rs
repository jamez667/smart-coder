//! Coverage: does the gateway actually reach the tool surface it claims to?
//!
//! This file exists because the first version of the crate silently reached
//! only 5 of the 18 registry tools — including, embarrassingly, none that could
//! produce test output, while the simplifier carried a purpose-built extractor
//! for exactly that. Nothing failed; the gap was simply invisible.
//!
//! So the split between "reachable through the gateway" and "deliberately not"
//! is written down here as an assertion rather than left to memory. Adding a
//! tool to the registry now forces a decision in this file.

use sc_gateway::table;

/// Tools that a `Need` must be able to reach.
///
/// These are the question-answering tools. Anything here that the gateway
/// cannot reach is a gap, not a policy — the whole premise is that one door
/// gets you to everything a model would otherwise pick from a menu.
const MUST_REACH: &[&str] = &[
    "read_file",
    "list_dir",
    "search_code",
    "find_symbol",
    "read_function",
    "cargo_info",
    "profile_hotspots",
    "run_verification",
];

/// Tools deliberately NOT reachable, each with the reason it is excluded.
///
/// The mutating ones share one reason: a classifier that can guess wrong must
/// never be able to guess into a write. The gateway answers questions; changing
/// the workspace stays on the discrete tool surface behind its permission gates.
const DELIBERATELY_ABSENT: &[(&str, &str)] = &[
    ("write_file", "mutating — a misroute would overwrite a file"),
    ("create_file", "mutating"),
    ("append_file", "mutating"),
    ("edit_file", "mutating"),
    ("edit_lines", "mutating"),
    ("edit_function", "mutating"),
    (
        "run_command",
        "destructive — confirm-gated, never router-reachable",
    ),
    ("update_plan", "agent loop state, not a query"),
    ("ask_user", "escalation — ends the run"),
    ("finish", "loop control, not a query"),
];

/// Every tool the default registry declares.
fn registry_tools() -> Vec<String> {
    sc_tools::default_registry()
        .specs()
        .iter()
        .map(|s| s.name.to_string())
        .collect()
}

/// Every tool the capability table routes to.
fn reachable_tools() -> Vec<String> {
    table()
        .iter()
        .filter_map(|c| c.backing_tool.map(str::to_string))
        .collect()
}

#[test]
fn every_read_only_tool_is_reachable() {
    let reachable = reachable_tools();
    for tool in MUST_REACH {
        assert!(
            reachable.iter().any(|r| r == tool),
            "{tool} is read-only but no capability reaches it — \
             either add a capability or move it to DELIBERATELY_ABSENT with a reason"
        );
    }
}

#[test]
fn no_mutating_tool_is_reachable() {
    // The security property of the whole design, asserted directly.
    let reachable = reachable_tools();
    for (tool, why) in DELIBERATELY_ABSENT {
        assert!(
            !reachable.iter().any(|r| r == tool),
            "{tool} became router-reachable, but is excluded because: {why}"
        );
    }
}

#[test]
fn the_two_lists_account_for_the_whole_registry() {
    // The test that would have caught the original gap. A tool added to the
    // registry lands in neither list and fails here, forcing a decision rather
    // than a silent omission.
    let declared = registry_tools();
    for tool in &declared {
        let claimed = MUST_REACH.contains(&tool.as_str())
            || DELIBERATELY_ABSENT.iter().any(|(t, _)| t == tool);
        assert!(
            claimed,
            "registry tool {tool} appears in neither MUST_REACH nor \
             DELIBERATELY_ABSENT — decide which it is"
        );
    }
}

#[test]
fn no_capability_points_at_a_tool_that_does_not_exist() {
    // A typo in `backing_tool` would otherwise surface only as a validation
    // error at run time, inside a capability the model cannot route around.
    let declared = registry_tools();
    for cap in table() {
        if let Some(tool) = cap.backing_tool {
            assert!(
                declared.iter().any(|d| d == tool),
                "{} points at {tool}, which the registry does not declare",
                cap.name
            );
        }
    }
}

#[test]
fn only_windowed_capabilities_claim_a_line_window() {
    // `windowed` drives whether the executor sends start/limit. Claiming it for
    // a tool whose schema has no such parameter turns every windowed need into
    // a validation error the model cannot repair.
    let registry = sc_tools::default_registry();
    for cap in table() {
        if !cap.windowed {
            continue;
        }
        let tool = cap
            .backing_tool
            .expect("a windowed capability must have a backing tool");
        let spec = registry.get(tool).expect("checked by another test");
        assert!(
            spec.param("start").is_some() && spec.param("limit").is_some(),
            "{} claims a line window but {tool} declares no start/limit",
            cap.name
        );
    }
}

/// The one tool the gateway reaches that the registry does not call read-only.
///
/// `run_verification` is `Mutating` because building and running a suite writes
/// to `target/` and executes the project's own test code. That is inherent to
/// running tests, not a property of routing to it — and a verify capability
/// that cannot run the suite is useless.
///
/// It is admitted on a narrower rule than "read-only": it cannot modify SOURCE.
/// It runs a command fixed by run configuration, never composed from the need,
/// so a misroute can waste time but cannot change the workspace's code.
const MUTATING_BUT_ADMITTED: &[&str] = &["run_verification"];

#[test]
fn no_reachable_tool_can_modify_source() {
    // Rather than trusting the name list above, ask the registry what each tool
    // actually does — this catches a tool that BECOMES mutating later while
    // keeping its name. Everything reachable must be read-only, except the
    // explicitly-reasoned exceptions above.
    let registry = sc_tools::default_registry();
    for cap in table() {
        let Some(tool) = cap.backing_tool else {
            continue;
        };
        if MUTATING_BUT_ADMITTED.contains(&tool) {
            continue;
        }
        let spec = registry.get(tool).expect("checked by another test");
        assert_eq!(
            spec.side_effect,
            sc_tools::SideEffect::ReadOnly,
            "{} reaches {tool}, which the registry classifies as {:?} — \
             admit it explicitly with a reason or drop the capability",
            cap.name,
            spec.side_effect
        );
    }
}

#[test]
fn nothing_destructive_is_ever_reachable() {
    // The exception above is deliberately narrow: `Mutating` can be argued for
    // case by case, `Destructive` never can. A shell the router could reach
    // would make every classifier bug a potential workspace loss.
    let registry = sc_tools::default_registry();
    for cap in table() {
        let Some(tool) = cap.backing_tool else {
            continue;
        };
        let spec = registry.get(tool).expect("checked by another test");
        assert_ne!(
            spec.side_effect,
            sc_tools::SideEffect::Destructive,
            "{} reaches {tool}, which is Destructive",
            cap.name
        );
    }
}

#[test]
fn the_admitted_exceptions_are_still_declared_reachable() {
    // An exception left behind after its capability was removed would quietly
    // widen what a future capability may reach.
    let reachable = reachable_tools();
    for tool in MUTATING_BUT_ADMITTED {
        assert!(
            reachable.iter().any(|r| r == tool),
            "{tool} is admitted as an exception but nothing reaches it — drop the exception"
        );
    }
}
