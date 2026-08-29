//! The mutating tools: create/write/append, and the three edit addressing modes.

use serde_json::json;

use super::{call, obs, temp_dir};
use crate::builtin::dispatch::execute;

#[test]
fn edit_function_replaces_the_whole_function() {
    // The Gunner scenario in miniature: add a match arm by rewriting the function.
    let ws = temp_dir("efn");
    let src = "\
enum Role { A, B }
fn pick(r: Role) -> u32 {
    match r {
        Role::A => 1,
        Role::B => 2,
    }
}
";
    std::fs::write(ws.join("m.rs"), src).unwrap();
    let new_body = "\
fn pick(r: Role) -> u32 {
    match r {
        Role::A => 1,
        Role::B => 2,
        Role::C => 3,
    }
}";
    let out = obs(execute(
        &call(json!({"tool":"edit_function","path":"m.rs","name":"pick","new_body":new_body})),
        &ws,
    ));
    assert!(out.contains("ok"), "edit ok: {out}");
    let after = std::fs::read_to_string(ws.join("m.rs")).unwrap();
    assert!(after.contains("Role::C => 3"), "new arm landed: {after}");
    assert!(after.contains("enum Role"), "rest of file intact: {after}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_function_missing_name_is_a_clear_error() {
    let ws = temp_dir("efn2");
    std::fs::write(ws.join("m.rs"), "fn a() {}\n").unwrap();
    let out = obs(execute(
        &call(json!({"tool":"edit_function","path":"m.rs","name":"nope","new_body":"fn nope(){}"})),
        &ws,
    ));
    assert!(out.contains("no function named `nope`"), "got: {out}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn create_file_writes_new_but_refuses_existing() {
    let ws = temp_dir("create");
    let c = call(json!({"tool":"create_file","path":"n.txt","content":"hi"}));
    assert!(obs(execute(&c, &ws)).contains("ok"));
    assert_eq!(std::fs::read_to_string(ws.join("n.txt")).unwrap(), "hi");
    // Second create on the same path is refused, not silently overwritten.
    let again = obs(execute(&c, &ws));
    assert!(again.contains("already exists"), "got: {again}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn write_file_refuses_to_overwrite_a_large_existing_file() {
    // The corruption guard: a model can't faithfully rewrite a big file, so overwriting one
    // with write_file is blocked and steered to surgical edits.
    let ws = temp_dir("write-big");
    let big: String = (0..200).map(|i| format!("fn f{i}() {{}}\n")).collect();
    std::fs::write(ws.join("big.rs"), &big).unwrap();
    let w = call(json!({"tool":"write_file","path":"big.rs","content":"fn only() {}"}));
    let o = obs(execute(&w, &ws));
    assert!(
        o.contains("rejected") && o.contains("too large"),
        "got: {o}"
    );
    // Untouched — the big file is preserved.
    assert_eq!(std::fs::read_to_string(ws.join("big.rs")).unwrap(), big);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn write_file_allows_new_and_small_files() {
    let ws = temp_dir("write-ok");
    // New file: fine.
    let n = call(json!({"tool":"write_file","path":"new.rs","content":"fn a() {}"}));
    assert!(obs(execute(&n, &ws)).contains("ok"));
    // Overwriting a SMALL existing file (≤150 lines): fine.
    let s = call(json!({"tool":"write_file","path":"new.rs","content":"fn b() {}"}));
    assert!(obs(execute(&s, &ws)).contains("ok"));
    assert_eq!(
        std::fs::read_to_string(ws.join("new.rs")).unwrap(),
        "fn b() {}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn append_file_creates_then_appends() {
    let ws = temp_dir("append");
    // First append creates the file.
    let a1 = call(json!({"tool":"append_file","path":"big.css","content":"a {}\n"}));
    assert!(obs(execute(&a1, &ws)).contains("ok"));
    // Second append adds to the end, not overwrites.
    let a2 = call(json!({"tool":"append_file","path":"big.css","content":"b {}\n"}));
    let o = obs(execute(&a2, &ws));
    assert!(o.contains("ok") && o.contains("total"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("big.css")).unwrap(),
        "a {}\nb {}\n",
        "append concatenates in order"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_replaces_a_unique_anchor() {
    let ws = temp_dir("edit-ok");
    std::fs::write(ws.join("a.rs"), "fn f() { return 1; }\n").unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.rs",
        "old_str":"return 1;","new_str":"return 2;"
    }));
    assert!(obs(execute(&e, &ws)).contains("1 replacement"));
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() { return 2; }\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_rejects_missing_anchor() {
    let ws = temp_dir("edit-miss");
    std::fs::write(ws.join("a.rs"), "fn f() {}\n").unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.rs","old_str":"nope","new_str":"x"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("0 matches"), "got: {o}");
    // File untouched.
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {}\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A MULTI-LINE ambiguous anchor must still show the model where the matches are.
///
/// The old filter was `line.contains(old_str)`, which can never be true when `old_str`
/// spans lines — no single line holds a newline. The message promised "copy a line
/// from below verbatim" and then showed nothing. Observed live on
/// `wireservice__csvkit-1281`: eight consecutive rejections on the same anchor, each
/// followed by an empty list.
#[test]
fn edit_file_shows_context_for_an_ambiguous_multiline_anchor() {
    let ws = temp_dir("edit-amb-multi");
    // The two-line anchor appears twice; the lines around it differ.
    std::fs::write(
        ws.join("a.py"),
        "def one():
    val = 1
    return val

def two():
    val = 1
    return val
",
    )
    .unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.py",
        "old_str":"    val = 1
    return val",
        "new_str":"    val = 2
    return val"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ambiguous"), "got: {o}");
    assert!(
        o.contains("line 1: def one():") && o.contains("line 5: def two():"),
        "both matches shown WITH the neighbouring line that tells them apart: {o}"
    );
    // Untouched — never edits on ambiguity.
    assert!(std::fs::read_to_string(ws.join("a.py"))
        .unwrap()
        .contains("val = 1"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_rejects_ambiguous_anchor() {
    let ws = temp_dir("edit-amb");
    std::fs::write(ws.join("a.rs"), "x\nx\n").unwrap();
    let e = call(json!({"tool":"edit_file","path":"a.rs","old_str":"x","new_str":"y"}));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("ambiguous") && o.contains("2 matches"),
        "got: {o}"
    );
    // The error lists each matching line so the model can pick a unique anchor.
    assert!(o.contains("line 1:") && o.contains("line 2:"), "got: {o}");
    // Untouched — never edits on ambiguity.
    assert_eq!(std::fs::read_to_string(ws.join("a.rs")).unwrap(), "x\nx\n");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_replaces_a_range_by_number() {
    // The large-file fix: address lines by NUMBER, no snippet to reproduce.
    let ws = temp_dir("edit-lines");
    std::fs::write(ws.join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":2,"end":3,"new_text":"TWO\nTHREE"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("ok") && o.contains("replaced lines 2..=3"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "one\nTWO\nTHREE\nfour\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_inserts_with_an_empty_range() {
    // end == start - 1 inserts BEFORE start without deleting.
    let ws = temp_dir("edit-lines-ins");
    std::fs::write(ws.join("a.rs"), "one\ntwo\n").unwrap();
    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":2,"end":1,"new_text":"INSERTED"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("ok") && o.contains("inserted before line 2"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "one\nINSERTED\ntwo\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_appends_at_end_of_file() {
    let ws = temp_dir("edit-lines-app");
    std::fs::write(ws.join("a.rs"), "one\ntwo\n").unwrap();
    // start = total+1, end = total → insert after the last line.
    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":3,"end":2,"new_text":"three"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "one\ntwo\nthree\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_rejects_out_of_range_with_a_self_correcting_error() {
    let ws = temp_dir("edit-lines-oor");
    std::fs::write(ws.join("a.rs"), "one\ntwo\n").unwrap();
    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":10,"end":12,"new_text":"x"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("out of range") && o.contains("2 lines"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "one\ntwo\n",
        "untouched"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_matches_a_crlf_anchor_against_a_crlf_file() {
    // THE Windows bug: the file is CRLF, the model copies a CRLF anchor from the shown file,
    // but edit_file used to normalize only the file → the `\r` in old_str broke the match and
    // every edit failed. Now both sides are normalized, so a CRLF anchor lands.
    let ws = temp_dir("edit-crlf");
    std::fs::write(ws.join("a.rs"), "fn f() {\r\n    let x = 1;\r\n}\r\n").unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.rs",
        "old_str":"    let x = 1;\r\n","new_str":"    let x = 2;\n"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("ok") || o.contains("replacement"),
        "CRLF anchor landed: {o}"
    );
    assert!(std::fs::read_to_string(ws.join("a.rs"))
        .unwrap()
        .contains("let x = 2;"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_whitespace_tolerant_multiline_match_lands() {
    // The large-file anchor-precision fix: the model reproduces a multi-line block's TEXT
    // but with different indentation/spacing, so byte-exact match fails. The fuzzy fallback
    // finds the real block and replaces it — the edit lands instead of the model thrashing.
    let ws = temp_dir("edit-fuzzy");
    std::fs::write(
        ws.join("a.rs"),
        "impl T {\n    pub fn generate(&self) -> u32 {\n        let x = 1;\n        x\n    }\n}\n",
    )
    .unwrap();
    // old_str has WRONG indentation (4 spaces flattened) but the right lines.
    let e = call(json!({
        "tool":"edit_file","path":"a.rs",
        "old_str":"pub fn generate(&self) -> u32 {\nlet x = 1;\nx\n}",
        "new_str":"pub fn generate(&self) -> u32 {\nself.build_lakes();\nlet x = 1;\nx\n}"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("whitespace-tolerant match"), "got: {o}");
    let got = std::fs::read_to_string(ws.join("a.rs")).unwrap();
    assert!(got.contains("self.build_lakes();"), "edit landed: {got}");
    // The new statement is indented to at least the matched block's level (4 spaces), not
    // left at column 0 (the model's flat new_str gets the block indent prefixed).
    assert!(
        got.contains("    self.build_lakes();"),
        "re-indented to block: {got}"
    );
    // The surrounding real lines are preserved.
    assert!(
        got.contains("let x = 1;") && got.contains("impl T {"),
        "kept context: {got}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_fuzzy_needs_a_unique_block() {
    // Two identical blocks → the fuzzy match is ambiguous → it does NOT fire (falls to the
    // error path), so we never edit the wrong one.
    let ws = temp_dir("edit-fuzzy-amb");
    std::fs::write(
        ws.join("a.rs"),
        "fn a() {\n  x;\n  y;\n}\nfn b() {\n  x;\n  y;\n}\n",
    )
    .unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.rs","old_str":"x;\ny;","new_str":"z;\ny;"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("not found") || o.contains("ambiguous"),
        "must not silently pick one: {o}"
    );
    // Untouched.
    assert!(std::fs::read_to_string(ws.join("a.rs"))
        .unwrap()
        .contains("  x;\n  y;\n}\nfn b"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_tolerates_literal_backslash_n_in_old_str() {
    // A small model writes "\\n" (literal backslash-n) instead of a real
    // newline in a multi-line old_str. The harness un-escapes and matches.
    let ws = temp_dir("edit-escn");
    std::fs::write(ws.join("m.py"), "def is_even(n):\n    return False\n").unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"m.py",
        "old_str":"def is_even(n):\\n    return False",
        "new_str":"def is_even(n):\\n    return n % 2 == 0"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("1 replacement"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("m.py")).unwrap(),
        "def is_even(n):\n    return n % 2 == 0\n",
        "real newlines applied, not literal backslash-n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_disambiguates_by_whole_line() {
    // "return n" substring-matches two lines, but as a whole trimmed line it
    // matches exactly one — the harness edits that line in place, preserving
    // indentation. (This is the mathlib `double` case from the live swarm.)
    let ws = temp_dir("edit-wholeline");
    std::fs::write(
        ws.join("m.py"),
        "def is_even(n):\n    return n % 2 == 0\n\n\ndef double(n):\n    return n\n",
    )
    .unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"m.py","old_str":"return n","new_str":"return n * 2"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("whole line"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("m.py")).unwrap(),
        "def is_even(n):\n    return n % 2 == 0\n\n\ndef double(n):\n    return n * 2\n",
        "only the double body line changed, indentation preserved"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
