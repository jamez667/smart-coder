//! The pre-write tripwires: duplicate definitions, brace balance, nested tool-call JSON.

use serde_json::json;

use super::{call, obs, temp_dir};
use crate::builtin::dispatch::execute;
use crate::builtin::guards::{duplicate_definition, top_level_defs};
use crate::builtin::write::{append_file, edit_lines};

#[test]
fn duplicate_definition_flags_a_re_emitted_fn() {
    let before = "pub fn a() {}\npub fn b() {}\n";
    // Adding a NEW fn is fine.
    assert!(duplicate_definition(before, &format!("{before}pub fn c() {{}}\n")).is_none());
    // Re-emitting an existing fn is a duplicate.
    let dup = duplicate_definition(before, &format!("{before}pub fn a() {{}}\n"));
    assert!(dup.is_some(), "re-defined `a` must be flagged");
    assert!(dup.unwrap().contains("`a`"));
    // structs/enums/traits too.
    assert!(duplicate_definition("struct S;", "struct S;\nstruct S;").is_some());
    // A pre-existing duplicate isn't blamed on an edit that doesn't worsen it.
    let pre_dup = "fn a() {}\nfn a() {}\n";
    assert!(duplicate_definition(pre_dup, &format!("{pre_dup}fn z() {{}}\n")).is_none());
}

#[test]
fn top_level_defs_ignores_nested_and_impl() {
    // Nested fns (indented) and impls are NOT top-level redefinitions.
    let src = "\
pub fn outer() {
    fn inner() {}
}
impl Foo { fn m(&self) {} }
impl Bar { fn m(&self) {} }
";
    let d = top_level_defs(src);
    assert_eq!(d.get("fn:outer").copied(), Some(1));
    assert!(!d.contains_key("fn:inner"), "nested fn ignored");
    assert!(!d.keys().any(|k| k.starts_with("impl")), "impl not counted");
}

#[test]
fn append_file_rejects_a_duplicate_and_allows_a_new_def() {
    let dir = temp_dir("append-dup");
    let existing = "pub fn draw_row() {}\npub fn draw_button() {}\n";
    std::fs::write(dir.join("w.rs"), existing).unwrap();
    // Re-appending an existing fn is rejected — file unchanged.
    let out = append_file(&dir, "w.rs", "\npub fn draw_row() {}\n");
    assert!(out.contains("rejected"), "dup append rejected: {out}");
    assert!(out.contains("draw_row"));
    assert_eq!(std::fs::read_to_string(dir.join("w.rs")).unwrap(), existing);
    // Appending a genuinely NEW fn is allowed.
    let out = append_file(&dir, "w.rs", "\npub fn draw_slider() {}\n");
    assert!(out.contains("ok"), "new append ok: {out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_lines_rejects_an_insert_that_duplicates_a_definition() {
    let dir = temp_dir("editlines-dup");
    std::fs::write(dir.join("w.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
    // Insert (end = start-1) a copy of `a` before line 2 → duplicate → rejected.
    let out = edit_lines(&dir, "w.rs", Some(2), Some(1), "pub fn a() {}");
    assert!(out.contains("rejected"), "dup insert rejected: {out}");
    assert!(out.contains("`a`"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_file_rejects_nested_tool_call_json_as_content() {
    // The lakes-render corruption: the model put its NEXT edit_file call in the content field.
    // Writing it would fill the .rs file with `{"tool":"edit_file",...}`. Guard rejects it.
    let ws = temp_dir("write-tooljson");
    std::fs::write(ws.join("a.rs"), "fn f() {}\n").unwrap();
    let nested = "{\n  \"tool\": \"edit_file\",\n  \"path\": \"b.rs\",\n  \"old_str\": \"x\"\n}";
    let e = call(json!({ "tool":"write_file","path":"a.rs","content": nested }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("rejected") && o.contains("tool-call JSON"),
        "got: {o}"
    );
    // File untouched — guard fires before the write.
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {}\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_rejects_embedded_tool_call_json() {
    // The stronger case: a real code prefix, THEN a nested tool-call object mid-content (the
    // shape that slipped past the prefix-only guard and corrupted mod.rs at line 49).
    let ws = temp_dir("edit-embed-json");
    std::fs::write(ws.join("a.rs"), "fn f() {\n    old();\n}\n").unwrap();
    let embedded =
        "fn f() {\n    new();\n}\n{\n  \"tool\": \"edit_file\",\n  \"path\": \"b.rs\"\n}";
    let e = call(json!({
        "tool":"edit_file","path":"a.rs","old_str":"fn f() {\n    old();\n}","new_str": embedded
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("rejected") && o.contains("tool-call JSON"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n    old();\n}\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn write_file_allows_real_code_that_mentions_tool() {
    // False-positive check: real source that happens to contain the word "tool" still writes.
    let ws = temp_dir("write-realcode");
    let e = call(json!({
        "tool":"write_file","path":"a.rs","content":"// pick a tool\nfn tool() {}\n"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok") || o.contains("wrote"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_rejects_a_brace_dropping_edit() {
    // The recurring render-stage failure: a range replacement that drops a closing brace.
    // The balance tripwire must reject it (file was balanced, edit unbalances it) instead of
    // writing broken code the model then thrashes on.
    let ws = temp_dir("edit-lines-brace");
    std::fs::write(
        ws.join("a.rs"),
        "fn f() {\n    if x {\n        g();\n    }\n}\n",
    )
    .unwrap();
    // Replace the inner block but "forget" the closing `}` of the if — net one unclosed `{`.
    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":2,"end":4,"new_text":"    if x {\n        g();"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("rejected") && o.contains("unclosed '{'"),
        "got: {o}"
    );
    // Steers to the INSERT form (the reliable fix for a brace-straddling replace).
    assert!(o.contains("INSERT"), "got: {o}");
    // File is untouched — the balance guard fires BEFORE the write.
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n    if x {\n        g();\n    }\n}\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_allows_a_balanced_edit() {
    // A range replacement that keeps delimiters balanced must go through (no false positive).
    let ws = temp_dir("edit-lines-ok");
    std::fs::write(ws.join("a.rs"), "fn f() {\n    old();\n}\n").unwrap();
    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":2,"end":2,"new_text":"    new(); more();"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}
