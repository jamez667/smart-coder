//! Editing a genuinely large file — the counterpart to `big_file_read.rs`.
//!
//! Reading an 8,000-line file now pages cleanly. This asks the next question:
//! once the model has found the line it wants, can it actually change it? Every
//! ladder fixture is small, so nothing in the eval suite exercises this.

use std::path::{Path, PathBuf};

fn workspace(lines: usize) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-bigedit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    let body: String = (1..=lines)
        .map(|i| format!("fn item_{i}() -> u32 {{ {i} }}\n"))
        .collect();
    std::fs::write(d.join("big.rs"), body).unwrap();
    d
}

fn run(ws: &Path, v: serde_json::Value) -> String {
    let call = sc_tools::default_registry()
        .validate(&v)
        .expect("valid tool call");
    match sc_tools::execute(&call, ws) {
        sc_tools::ToolOutcome::Observation(o) => o,
        _ => panic!("expected an observation"),
    }
}

/// An anchored edit deep inside a large file lands.
#[test]
fn an_anchored_edit_at_line_6000_lands() {
    let ws = workspace(8000);
    let out = run(
        &ws,
        serde_json::json!({
            "tool": "edit_file",
            "path": "big.rs",
            "old_str": "fn item_6000() -> u32 { 6000 }",
            "new_str": "fn item_6000() -> u32 { 424242 }"
        }),
    );
    assert!(
        !out.to_lowercase().contains("not found"),
        "the anchor must match deep in a big file, got: {out}"
    );
    let after = std::fs::read_to_string(ws.join("big.rs")).unwrap();
    assert!(after.contains("424242"), "the edit must be on disk");
    assert_eq!(
        after.lines().count(),
        8000,
        "an anchored edit must not change the file's length"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// `edit_lines` addressed by number works deep in a large file — the model gets
/// those numbers from `read_file`'s `N: ` view, so this is the paged-read pairing.
#[test]
fn a_line_addressed_edit_at_line_6000_lands() {
    let ws = workspace(8000);
    let out = run(
        &ws,
        serde_json::json!({
            "tool": "edit_lines",
            "path": "big.rs",
            "start": 6000,
            "end": 6000,
            "new_text": "fn item_6000() -> u32 { 999 }"
        }),
    );
    assert!(
        !out.to_lowercase().contains("error"),
        "a line-addressed edit must work deep in a big file, got: {out}"
    );
    let after = std::fs::read_to_string(ws.join("big.rs")).unwrap();
    let line = after.lines().nth(5999).unwrap();
    assert!(
        line.contains("999"),
        "line 6000 must be the one that changed, got: {line}"
    );
    assert_eq!(after.lines().count(), 8000, "length is unchanged");
    let _ = std::fs::remove_dir_all(&ws);
}

/// A whole-file overwrite of a large file is REFUSED, and the refusal must point
/// at a tool that actually works on a file this size.
#[test]
fn a_whole_file_overwrite_is_refused_with_a_usable_alternative() {
    let ws = workspace(8000);
    let out = run(
        &ws,
        serde_json::json!({
            "tool": "write_file",
            "path": "big.rs",
            "content": "fn item_1() -> u32 { 1 }\n"
        }),
    );
    assert!(
        out.contains("too large to safely overwrite"),
        "overwriting 8,000 lines wholesale must be refused, got: {out}"
    );
    // The refusal is only useful if it names a way forward.
    assert!(
        out.contains("edit_file") || out.contains("edit_lines") || out.contains("append_file"),
        "the refusal must name a tool that works at this size, got: {out}"
    );
    let after = std::fs::read_to_string(ws.join("big.rs")).unwrap();
    assert_eq!(after.lines().count(), 8000, "the file must be untouched");
    let _ = std::fs::remove_dir_all(&ws);
}
