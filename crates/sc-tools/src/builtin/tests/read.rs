//! The read-only navigation tools: windowed reads, function reads, listing, search.

use serde_json::json;

use super::{call, obs, temp_dir};
use crate::builtin::dispatch::{execute, ToolOutcome};
use crate::builtin::read::READ_FILE_DEFAULT_LINES;

#[test]
fn read_function_returns_just_that_function() {
    let ws = temp_dir("rfn");
    let src = "fn a() { 1 }\n\nfn target(x: u32) -> u32 {\n    x + 1\n}\n\nfn b() {}\n";
    std::fs::write(ws.join("lib.rs"), src).unwrap();
    let out = obs(execute(
        &call(json!({"tool":"read_function","path":"lib.rs","name":"target"})),
        &ws,
    ));
    assert!(out.contains("fn target"), "got: {out}");
    assert!(out.contains("x + 1"), "body present: {out}");
    assert!(!out.contains("fn a("), "only the target function: {out}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn read_file_windows_to_a_line_range() {
    let ws = temp_dir("rwin");
    let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
    std::fs::write(ws.join("big.txt"), body).unwrap();
    // start=10, limit=3 → lines 10,11,12 only.
    let r = call(json!({"tool":"read_file","path":"big.txt","start":10,"limit":3}));
    let o = obs(execute(&r, &ws));
    assert!(o.contains("lines 10-12 of 50"), "labels the window: {o}");
    assert!(
        o.contains("line 10") && o.contains("line 12"),
        "window content: {o}"
    );
    assert!(
        !o.contains("line 9\n") && !o.contains("line 13"),
        "outside window excluded: {o}"
    );
    // The continuation hint tells the model how to read the next chunk.
    assert!(o.contains("\"start\":13"), "next-chunk hint: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn read_file_caps_a_large_file_by_default() {
    let ws = temp_dir("rcap");
    let body: String = (1..=1000).map(|n| format!("L{n}\n")).collect();
    std::fs::write(ws.join("huge.txt"), body).unwrap();
    let r = call(json!({"tool":"read_file","path":"huge.txt"}));
    let o = obs(execute(&r, &ws));
    // Only the first READ_FILE_DEFAULT_LINES are returned, with a continuation hint.
    assert!(
        o.contains(&format!("lines 1-{READ_FILE_DEFAULT_LINES} of 1000")),
        "capped: {o}"
    );
    assert!(o.contains("more line(s)"), "truncation noted: {o}");
    assert!(!o.contains("L1000"), "tail not included: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn search_code_skips_the_agents_own_session_logs() {
    let ws = temp_dir("rsearch");
    std::fs::create_dir_all(ws.join(".smart-coder/sessions")).unwrap();
    // The needle appears in BOTH a session log and a real source file.
    std::fs::write(
        ws.join(".smart-coder/sessions/x.jsonl"),
        "stringify_reason in a log",
    )
    .unwrap();
    std::fs::write(ws.join("real.rs"), "fn stringify_reason() {}").unwrap();
    let s = call(json!({"tool":"search_code","query":"stringify_reason"}));
    let o = obs(execute(&s, &ws));
    assert!(o.contains("real.rs"), "finds the source: {o}");
    assert!(
        !o.contains(".smart-coder"),
        "does not match its own log: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn list_dir_sorts_and_marks_directories() {
    let ws = temp_dir("ls");
    std::fs::create_dir(ws.join("zdir")).unwrap();
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let o = match execute(&call(json!({"tool":"list_dir","path":"."})), &ws) {
        ToolOutcome::Observation(o) => o,
        _ => panic!(),
    };
    let body = o.split_once('\n').unwrap().1;
    assert_eq!(body, "a.txt\nzdir/");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn search_code_finds_literal_hits_with_line_numbers() {
    let ws = temp_dir("search");
    std::fs::write(ws.join("a.rs"), "fn one() {}\nfn target() {}\n").unwrap();
    std::fs::write(ws.join("b.rs"), "nothing here\n").unwrap();
    let o = match execute(&call(json!({"tool":"search_code","query":"target"})), &ws) {
        ToolOutcome::Observation(o) => o,
        _ => panic!(),
    };
    assert!(o.contains("a.rs:2"), "got: {o}");
    assert!(!o.contains("b.rs"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn search_code_matches_regex_patterns() {
    let ws = temp_dir("searchre");
    std::fs::write(
        ws.join("a.rs"),
        "fn alpha() {}\nfn beta_two() {}\nlet x = ShipRole::Miner;\n",
    )
    .unwrap();
    // `fn \w+` matches both function lines via regex (would be literal-nomatch before).
    let o = obs(execute(
        &call(json!({"tool":"search_code","query":r"fn \w+"})),
        &ws,
    ));
    assert!(
        o.contains("a.rs:1") && o.contains("a.rs:2"),
        "regex fn: {o}"
    );
    // `ShipRole::\w+` finds the enum use.
    let o2 = obs(execute(
        &call(json!({"tool":"search_code","query":r"ShipRole::\w+"})),
        &ws,
    ));
    assert!(o2.contains("a.rs:3"), "regex enum use: {o2}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn search_code_falls_back_to_literal_for_invalid_regex() {
    let ws = temp_dir("searchlit");
    // `[` alone is invalid regex — must fall back to a literal substring search, not error.
    std::fs::write(ws.join("a.rs"), "let v = arr[0];\nno bracket here\n").unwrap();
    let o = obs(execute(
        &call(json!({"tool":"search_code","query":"arr["})),
        &ws,
    ));
    assert!(
        o.contains("a.rs:1"),
        "literal fallback for invalid regex: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn search_code_reports_no_matches() {
    let ws = temp_dir("search-none");
    std::fs::write(ws.join("a.rs"), "x\n").unwrap();
    let o = match execute(&call(json!({"tool":"search_code","query":"zzz"})), &ws) {
        ToolOutcome::Observation(o) => o,
        _ => panic!(),
    };
    assert!(o.contains("no matches"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}
