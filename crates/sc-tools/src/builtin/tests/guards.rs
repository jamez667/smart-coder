//! The pre-write tripwires: duplicate definitions, brace balance, nested tool-call JSON.

use serde_json::json;

use super::{call, obs, temp_dir};
use crate::builtin::dispatch::execute;
use crate::builtin::guards::{duplicate_definition, top_level_defs};
use crate::builtin::write::{append_file, edit_file, edit_lines};

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

// ---------------------------------------------------------------------------
// The destructive-replacement guard: `edit_file` writing bare punctuation over
// a real statement (the live rung failure), and every legitimate edit it must
// leave alone.
// ---------------------------------------------------------------------------

use crate::builtin::guards::destructive_replacement;

/// Run an `edit_file` through the real dispatch path and return (observation, file after).
fn run_edit(tag: &str, name: &str, src: &str, old: &str, new: &str) -> (String, String) {
    let ws = temp_dir(tag);
    std::fs::write(ws.join(name), src).unwrap();
    let o = obs(execute(
        &call(json!({"tool":"edit_file","path":name,"old_str":old,"new_str":new})),
        &ws,
    ));
    let after = std::fs::read_to_string(ws.join(name)).unwrap();
    let _ = std::fs::remove_dir_all(&ws);
    (o, after)
}

/// THE LIVE FAILURE. Turn 2 of a real run sent
/// `{"old_str":"        here = here.max(v);","new_str":":"}`. `edit_file` wrote it, said "ok",
/// and line 10 of lib.rs became `:`. Every later turn then fought a build broken by turn 2.
#[test]
fn edit_file_rejects_a_statement_replaced_by_bare_punctuation() {
    let src = "\
pub fn longest(v: &[u32]) -> u32 {
    let mut here = 0;
    for &v in v {
        here = here.max(v);
    }
    here
}
";
    let (o, after) = run_edit(
        "destructive-colon",
        "lib.rs",
        src,
        "here = here.max(v);",
        ":",
    );
    assert!(o.contains("rejected"), "must be rejected: {o}");
    assert!(o.contains("DESTROY"), "names the failure mode: {o}");
    assert!(o.contains("here = here.max(v);"), "names the anchor: {o}");
    assert!(
        o.contains("FULL replacement statement"),
        "says what to send: {o}"
    );
    assert_eq!(after, src, "the file must NOT be written");
}

/// Deleting a line by replacing it with `""` is a real operation and must keep working.
///
/// Driven at the `edit_file` fn, not through dispatch: the registry validator already refuses an
/// empty `new_str` before a call reaches a writer (`ValidationError::EmptyString`), so a model
/// cannot spell a deletion this way today. `edit_file` is public and must still honour it — and
/// the new guard must not be the thing that stops it.
#[test]
fn edit_file_still_deletes_a_line_with_an_empty_new_str() {
    let ws = temp_dir("destructive-del");
    std::fs::write(
        ws.join("a.rs"),
        "fn f() {
    dbg!(x);
    g();
}
",
    )
    .unwrap();
    let o = edit_file(
        &ws,
        "a.rs",
        "    dbg!(x);
",
        "",
    );
    assert!(o.contains("ok"), "deletion is legitimate: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {
    g();
}
"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A shorter statement replacing a longer one: the ordinary edit the guard must never touch.
#[test]
fn edit_file_allows_a_shorter_statement() {
    let src = "fn f(v: u32) {\n    here = here.max(v);\n}\n";
    let (o, after) = run_edit(
        "destructive-shorter",
        "a.rs",
        src,
        "here = here.max(v);",
        "here = v;",
    );
    assert!(o.contains("ok"), "shorter statement is fine: {o}");
    assert!(after.contains("here = v;"), "{after}");
}

/// Replacing code with a comment. `new_str` has letters, so the guard never engages.
#[test]
fn edit_file_allows_replacing_code_with_a_comment() {
    let src = "fn f(v: u32) {\n    here = here.max(v);\n}\n";
    let (o, after) = run_edit(
        "destructive-comment",
        "a.rs",
        src,
        "here = here.max(v);",
        "// removed",
    );
    assert!(o.contains("ok"), "a comment is fine: {o}");
    assert!(after.contains("// removed"), "{after}");
}

/// A tiny operator tweak — punctuation for punctuation. `old_str` carries no real code, so the
/// "you destroyed a statement" premise never holds.
#[test]
fn edit_file_allows_an_operator_tweak() {
    let src = "fn f(a: u32, b: u32) -> bool {\n    a >= b\n}\n";
    let (o, after) = run_edit("destructive-op", "a.rs", src, ">=", ">");
    assert!(o.contains("ok"), "operator tweak is fine: {o}");
    assert!(after.contains("a > b"), "{after}");
    // And the same tweak spelled with its operands still goes through (4 alnum or fewer).
    let (o, after) = run_edit("destructive-op2", "a.rs", src, "a >= b", "a > b");
    assert!(o.contains("ok"), "operand form is fine too: {o}");
    assert!(after.contains("a > b"), "{after}");
}

/// Collapsing a block down to its closing brace. Bare punctuation, but structurally real code.
#[test]
fn edit_file_allows_collapsing_a_block_to_a_closing_brace() {
    let src = "fn f() {\n    if x {\n        long_call(1);\n    }\n}\n";
    let (o, after) = run_edit(
        "destructive-brace",
        "a.rs",
        src,
        "        long_call(1);\n    }",
        "    }",
    );
    assert!(o.contains("ok"), "closing brace is legitimate: {o}");
    assert_eq!(after, "fn f() {\n    if x {\n    }\n}\n");
}

/// Non-code paths are untouched by the guard: a `.md` may legitimately become a bullet.
#[test]
fn destructive_guard_skips_non_code_paths() {
    let src = "# Notes\n\nsome long prose line here\n";
    let (o, after) = run_edit(
        "destructive-md",
        "n.md",
        src,
        "some long prose line here",
        "---",
    );
    assert!(o.contains("ok"), "markdown is not guarded: {o}");
    assert!(after.contains("---"), "{after}");
}

/// The tripwire wiring: `edit_file` now runs `delimiter_regression` like the other editors.
#[test]
fn edit_file_rejects_a_brace_dropping_replacement() {
    let src = "fn f() {\n    if x {\n        g();\n    }\n}\n";
    let (o, after) = run_edit(
        "destructive-delim",
        "a.rs",
        src,
        "    if x {\n        g();\n    }",
        "    if x {\n        g();",
    );
    assert!(o.contains("rejected"), "must be rejected: {o}");
    assert!(o.contains("unclosed '{'"), "names the delimiter: {o}");
    assert_eq!(after, src, "the file must NOT be written");
}

/// The predicate directly, so the boundary cases are pinned without a filesystem round-trip.
#[test]
fn destructive_replacement_predicate_boundaries() {
    // The live case.
    assert!(destructive_replacement("        here = here.max(v);", ":").is_some());
    // Other bare-punctuation nonsense over a real statement.
    for bad in [":", "?", "=", "::", ":;", "!", "#", "@", "<", "->"] {
        assert!(
            destructive_replacement("let total = compute(a, b);", bad).is_some(),
            "{bad:?} over a statement must be rejected"
        );
    }
    // Never fires on an empty new_str (deletion), or anything carrying letters/digits.
    for ok in ["", "   ", "here = v;", "// removed", "0", "_x", "a > b"] {
        assert!(
            destructive_replacement("here = here.max(v);", ok).is_none(),
            "{ok:?} must be accepted"
        );
    }
    // Structural scraps: closing a block is legitimate punctuation.
    for ok in [
        "}", "};", "},", "})", "});", ")", ");", "]", "],", ",", "{}",
    ] {
        assert!(
            destructive_replacement("here = here.max(v);", ok).is_none(),
            "{ok:?} is a structural scrap, not destruction"
        );
    }
    // A punctuation-for-punctuation tweak: old_str carries no real code, so no statement is lost.
    for (old, new) in [(">=", ">"), ("&&", "||"), ("==", "!="), ("a >= b", "a > b")] {
        assert!(
            destructive_replacement(old, new).is_none(),
            "{old:?} -> {new:?} is a tweak, not destruction"
        );
    }
}
