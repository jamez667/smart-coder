//! Direct tools vs the gateway, on the same needs.
//!
//! **The check that found the bugs the fixture suite did not.** Every other test
//! here asserts what the gateway returns; this asserts it returns THE SAME THING
//! a caller would get by calling the tool directly. Three real bugs surfaced the
//! first two times it was run:
//!
//! * `file.list` ignored a directory named in the text and listed the repo root;
//! * `code.function` rejected a plain lowercase name like `classify`;
//! * `code.search` leaked the word "codebase" into the query and found nothing.
//!
//! None of them failed any existing test, because the fixtures only exercised
//! the paths their author had thought of.

use std::path::Path;

use sc_gateway::{Ctx, Gateway, Need};

/// Newline, as a value — keeps the escape out of a generated string literal.
const NL: char = '\n';

/// One need, expressed as a direct tool call and as plain English.
struct Pair {
    what: &'static str,
    direct: serde_json::Value,
    ask: &'static str,
    scope: Option<&'static str>,
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/agent")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]
name = \"demo\"
version = \"0.1.0\"
",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod agent;

/// The entry point.
pub fn run_demo() -> usize {
    42
}
",
    )
    .unwrap();
    std::fs::write(
        root.join("src/agent/stall.rs"),
        "/// Detect a stalled agent loop.
pub fn handle_timeout(ticks: usize) -> bool {
    ticks > 3
}

pub struct StallDetector {
    pub ticks: usize,
}
",
    )
    .unwrap();
    dir
}

/// Run a direct call the way the AGENT LOOP would — including the tools
/// `sc-tools` declares but does not execute. Routing `find_symbol` here rather
/// than letting it report "no executor" is the difference between comparing the
/// gateway against the real tool surface and comparing it against a stub.
fn direct(call: &sc_tools::ValidatedCall, root: &Path) -> String {
    if call.name == "find_symbol" {
        return sc_index::find_symbol(root, call.str("name").unwrap_or_default());
    }
    match sc_tools::execute(call, root) {
        sc_tools::ToolOutcome::Observation(t) => t,
        sc_tools::ToolOutcome::Finished => "(finished)".to_string(),
    }
}

#[test]
fn the_gateway_returns_what_the_direct_tools_return() {
    let dir = fixture();
    let root = dir.path();
    let registry = sc_tools::default_registry();
    let gw = Gateway::new();
    let ctx = Ctx {
        workspace: root,
        model: None,
        web: None,
        verify: None,
    };

    let pairs = [
        Pair {
            what: "read a file",
            direct: serde_json::json!({"tool":"read_file","path":"src/lib.rs"}),
            ask: "read src/lib.rs",
            scope: None,
        },
        Pair {
            what: "read a file, conversational",
            direct: serde_json::json!({"tool":"read_file","path":"src/agent/stall.rs"}),
            ask: "can you show me the full contents of src/agent/stall.rs please",
            scope: None,
        },
        Pair {
            what: "read part of a file",
            direct: serde_json::json!({"tool":"read_file","path":"src/agent/stall.rs","start":1,"limit":4}),
            ask: "show me lines 1 through 4 of src/agent/stall.rs",
            scope: None,
        },
        Pair {
            what: "list a directory",
            direct: serde_json::json!({"tool":"list_dir","path":"src/agent"}),
            ask: "list the files in src/agent",
            scope: None,
        },
        Pair {
            what: "search for an identifier",
            direct: serde_json::json!({"tool":"search_code","query":"handle_timeout"}),
            ask: "please find all occurrences of handle_timeout in the codebase",
            scope: None,
        },
        Pair {
            what: "find a definition",
            direct: serde_json::json!({"tool":"find_symbol","name":"StallDetector"}),
            ask: "where is StallDetector defined",
            scope: None,
        },
        Pair {
            what: "read one function",
            direct: serde_json::json!({"tool":"read_function","path":"src/agent/stall.rs","name":"handle_timeout"}),
            ask: "show me the body of the handle_timeout function",
            scope: Some("src/agent/stall.rs"),
        },
        Pair {
            what: "crate facts",
            direct: serde_json::json!({"tool":"cargo_info","crate":"demo"}),
            ask: "what does the demo crate depend on",
            scope: None,
        },
    ];

    let mut mismatches = Vec::new();
    for p in &pairs {
        let call = registry.validate(&p.direct).expect("direct call validates");
        let expected = direct(&call, root);
        let need = match p.scope {
            Some(s) => Need::scoped(p.ask, s),
            None => Need::new(p.ask),
        };
        let answer = gw.ask_with(&need, &ctx);
        if expected.trim() != answer.text.trim() {
            mismatches.push(format!(
                "{}:
    direct: {:.90}
    ask   : {:.90}",
                p.what,
                expected.replace(NL, " | "),
                answer.text.replace(NL, " | ")
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} needs differ between the direct tools and the gateway:
{}",
        mismatches.len(),
        pairs.len(),
        mismatches.join(
            "
"
        )
    );
}
