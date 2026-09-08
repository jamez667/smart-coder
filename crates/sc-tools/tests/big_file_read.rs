//! What a model actually gets back when it reads a genuinely large file.
//!
//! Reported from real use: on an 8,000-line file the agent "fails to read to line
//! 2800" while pi manages it. These tests pin the boundaries so the answer is a
//! measured number rather than an argument.

use std::path::PathBuf;

fn workspace() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-bigfile-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    let body: String = (1..=8000)
        .map(|i| format!("// line {i}: representative source content on this line\n"))
        .collect();
    std::fs::write(d.join("big.rs"), body).unwrap();
    d
}

/// Drive `read_file` the way the model does: through the registry + public executor.
fn read(ws: &std::path::Path, start: Option<i64>, limit: Option<i64>) -> String {
    let mut v = serde_json::json!({"tool": "read_file", "path": "big.rs"});
    if let Some(s) = start {
        v["start"] = serde_json::json!(s);
    }
    if let Some(l) = limit {
        v["limit"] = serde_json::json!(l);
    }
    let call = sc_tools::default_registry()
        .validate(&v)
        .expect("valid read_file call");
    match sc_tools::execute(&call, ws) {
        sc_tools::ToolOutcome::Observation(o) => o,
        _ => panic!("read_file must return an observation"),
    }
}

/// Without an explicit limit, a read stops at 400 lines and SAYS so.
#[test]
fn an_unwindowed_read_of_a_huge_file_stops_at_the_default_and_says_how_to_continue() {
    let ws = workspace();
    let out = read(&ws, None, None);

    assert!(
        out.contains("(lines 1-400 of 8000)"),
        "the header must state the window and the true total, got:\n{}",
        &out[..out.len().min(200)]
    );
    assert!(
        out.contains("pass start=401 for more"),
        "it must tell the model how to continue, got:\n{}",
        &out[out.len().saturating_sub(200)..]
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Reading AT line 2800 works — the reported failure is not about that offset.
#[test]
fn a_windowed_read_at_line_2800_returns_that_window() {
    let ws = workspace();
    let out = read(&ws, Some(2800), Some(50));

    assert!(
        out.contains("(lines 2800-2849 of 8000)"),
        "reading at an offset deep in the file must work, got:\n{}",
        &out[..out.len().min(200)]
    );
    assert!(
        out.contains("2800: // line 2800"),
        "the window starts where asked"
    );
    assert!(
        out.contains("2849: // line 2849"),
        "the window ends where asked"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// The tool itself will hand back a very large window if asked. Whether the LOOP
/// then keeps it is a separate question (`read_file_line_cap`), which is what the
/// reported failure is really about.
#[test]
fn the_tool_honours_a_large_explicit_limit() {
    let ws = workspace();
    let out = read(&ws, Some(1), Some(8000));

    assert!(
        out.contains("(8000 lines)") || out.contains("lines 1-8000 of 8000"),
        "an explicit whole-file read is honoured by the tool, got:\n{}",
        &out[..out.len().min(200)]
    );
    assert!(
        out.contains("8000: // line 8000"),
        "the last line is present"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
