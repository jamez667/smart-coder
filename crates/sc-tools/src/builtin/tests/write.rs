//! The mutating tools: create/write/append, and the three edit addressing modes.

use serde_json::json;

use super::{call, obs, temp_dir};
use crate::builtin::dispatch::execute;
use crate::builtin::registry::default_registry;

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
    assert!(o.contains("anchor not found"), "got: {o}");
    // File untouched.
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {}\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **A missed anchor shows the closest block, never the whole file.**
///
/// The old miss message dumped the entire numbered file: on a 900-line file that cost
/// the model its window and said nothing about WHERE it had been looking. Now it gets
/// the few lines around the line that most resembles its anchor, with line numbers.
#[test]
fn edit_file_miss_shows_the_closest_block_not_the_whole_file() {
    let ws = temp_dir("edit-miss-closest");
    let body: String = (1..=100).map(|n| format!("let v{n} = {n};\n")).collect();
    std::fs::write(ws.join("big.rs"), &body).unwrap();
    // Wrong on the value, right on the shape: nearest to line 50.
    let e = call(json!({
        "tool":"edit_file","path":"big.rs",
        "old_str":"let v50 = 999;","new_str":"let v50 = 0;"
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.starts_with("edit_file big.rs: anchor not found; closest match:\n"),
        "got: {o}"
    );
    // ±3 lines around the closest line, each numbered `N: `.
    for n in 47..=53 {
        assert!(
            o.contains(&format!("\n{n}: let v{n} = {n};")),
            "line {n}: {o}"
        );
    }
    assert!(!o.contains("\n1: let v1 "), "never the whole file: {o}");
    assert!(!o.contains("\n100: "), "never the whole file: {o}");
    assert!(o.lines().count() <= 31, "header + at most 30 lines: {o}");
    // Untouched.
    assert_eq!(std::fs::read_to_string(ws.join("big.rs")).unwrap(), body);
    let _ = std::fs::remove_dir_all(&ws);
}

/// A long anchor that misses still yields a bounded message.
#[test]
fn edit_file_miss_output_is_capped_at_thirty_lines() {
    let ws = temp_dir("edit-miss-cap");
    let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
    std::fs::write(ws.join("big.txt"), &body).unwrap();
    // A 60-line anchor whose first line resembles line 100 but whose body is wrong.
    let anchor: String = (0..60).map(|i| format!("line {} x\n", 100 + i)).collect();
    let e = call(json!({"tool":"edit_file","path":"big.txt","old_str":anchor,"new_str":"y"}));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("anchor not found; closest match:"), "got: {o}");
    assert!(
        o.lines().count() <= 31,
        "header + at most 30 lines, got {}: {o}",
        o.lines().count()
    );
    assert!(
        o.contains("\n97: line 97"),
        "starts 3 before the match: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **A single-line anchor lands despite indentation drift.**
///
/// The whitespace-tolerant match used to need two lines; a one-line anchor with the
/// wrong indent simply missed, and the model re-read the file to try again.
#[test]
fn edit_file_single_line_anchor_tolerates_indent_drift() {
    let ws = temp_dir("edit-1line-indent");
    std::fs::write(ws.join("a.rs"), "fn f() {\n        let x = 1;\n}\n").unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.rs","old_str":"let x = 1;","new_str":"let x = 2;"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n        let x = 2;\n}\n",
        "replaced at the file's own indentation"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_single_line_anchor_tolerates_tabs_vs_spaces() {
    let ws = temp_dir("edit-1line-tabs");
    std::fs::write(ws.join("a.rs"), "fn f() {\n\tlet x = 1;\n}\n").unwrap();
    // The model writes four spaces where the file has a tab.
    let e = call(json!({
        "tool":"edit_file","path":"a.rs","old_str":"    let x = 1;","new_str":"    let x = 2;"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n\tlet x = 2;\n}\n",
        "the file keeps its tab"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_file_single_line_anchor_tolerates_trailing_whitespace() {
    let ws = temp_dir("edit-1line-trail");
    std::fs::write(ws.join("a.rs"), "fn f() {\n    let x = 1;\n}\n").unwrap();
    let e = call(json!({
        "tool":"edit_file","path":"a.rs","old_str":"    let x = 1;   ","new_str":"    let x = 2;"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n    let x = 2;\n}\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A single-line fuzzy match must still be UNIQUE; two candidates mean no edit.
#[test]
fn edit_file_single_line_fuzzy_match_must_be_unique() {
    let ws = temp_dir("edit-1line-amb");
    std::fs::write(ws.join("a.rs"), "  x = 1;\n    x = 1;\n").unwrap();
    let e = call(json!({"tool":"edit_file","path":"a.rs","old_str":"x = 1;  ","new_str":"x = 2;"}));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("not found") || o.contains("ambiguous"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "  x = 1;\n    x = 1;\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// True when every line ending in `s` is CRLF (and there is at least one).
fn all_crlf(s: &str) -> bool {
    s.contains("\r\n") && !s.replace("\r\n", "").contains('\n')
}

/// **An edit never flips a CRLF file to LF.**
///
/// Every editor used to write LF regardless of what it read, so one small edit on a
/// Windows checkout became a whole-file diff and the next `git diff` was unreadable.
#[test]
fn edit_file_preserves_crlf_line_endings() {
    let ws = temp_dir("crlf-edit-file");
    std::fs::write(ws.join("a.rs"), "fn f() {\r\n    let x = 1;\r\n}\r\n").unwrap();
    // Exact, fuzzy and whole-line paths all write back CRLF.
    for (old, new) in [
        ("let x = 1;", "let x = 2;"),         // exact substring
        ("        let x = 2;", "let x = 3;"), // fuzzy (indent drift)
    ] {
        let e = call(json!({"tool":"edit_file","path":"a.rs","old_str":old,"new_str":new}));
        let o = obs(execute(&e, &ws));
        assert!(o.contains("ok"), "got: {o}");
        let got = std::fs::read_to_string(ws.join("a.rs")).unwrap();
        assert!(all_crlf(&got), "CRLF kept after {old:?}: {got:?}");
        assert!(got.contains(new), "edit landed: {got:?}");
    }
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_lines_preserves_crlf_and_leaves_lf_alone() {
    let ws = temp_dir("crlf-edit-lines");
    std::fs::write(ws.join("c.rs"), "one\r\ntwo\r\nthree\r\n").unwrap();
    let e =
        call(json!({"tool":"edit_lines","path":"c.rs","start":2,"end":2,"new_text":"TWO\nTWO-B"}));
    assert!(obs(execute(&e, &ws)).contains("ok"));
    assert_eq!(
        std::fs::read_to_string(ws.join("c.rs")).unwrap(),
        "one\r\nTWO\r\nTWO-B\r\nthree\r\n"
    );
    // An LF file stays LF.
    std::fs::write(ws.join("l.rs"), "one\ntwo\n").unwrap();
    let e = call(json!({"tool":"edit_lines","path":"l.rs","start":2,"end":2,"new_text":"TWO"}));
    assert!(obs(execute(&e, &ws)).contains("ok"));
    assert_eq!(
        std::fs::read_to_string(ws.join("l.rs")).unwrap(),
        "one\nTWO\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn edit_function_preserves_crlf_line_endings() {
    let ws = temp_dir("crlf-edit-fn");
    std::fs::write(
        ws.join("m.rs"),
        "fn a() {}\r\nfn pick() -> u32 {\r\n    1\r\n}\r\nfn b() {}\r\n",
    )
    .unwrap();
    let e = call(json!({
        "tool":"edit_function","path":"m.rs","name":"pick",
        "new_body":"fn pick() -> u32 {\n    2\n}"
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("ok"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("m.rs")).unwrap(),
        "fn a() {}\r\nfn pick() -> u32 {\r\n    2\r\n}\r\nfn b() {}\r\n"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn append_file_preserves_crlf_line_endings() {
    let ws = temp_dir("crlf-append");
    std::fs::write(ws.join("a.css"), "a {}\r\n").unwrap();
    let e = call(json!({"tool":"append_file","path":"a.css","content":"b {}\nc {}\n"}));
    assert!(obs(execute(&e, &ws)).contains("ok"));
    assert_eq!(
        std::fs::read_to_string(ws.join("a.css")).unwrap(),
        "a {}\r\nb {}\r\nc {}\r\n"
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

// --- No-op writes: a write that changes no bytes must SAY so --------------------
//
// THE BUG. `edit_file` computed `content.replacen(old, new, 1)` and answered
// "ok (1 replacement)" without ever comparing the result to what it started with. A
// model that submitted `old_str == new_str` was told its edit landed. Measured on one
// Mellum run: four verbatim no-op turns, each answered "ok", and the model spent the
// rest of the run reasoning from a false premise. Every writer is checked here.

/// Read a file's bytes and mtime together — the pair a no-op must leave untouched.
fn stamp(p: &std::path::Path) -> (String, std::time::SystemTime) {
    (
        std::fs::read_to_string(p).unwrap(),
        std::fs::metadata(p).unwrap().modified().unwrap(),
    )
}

/// A no-op observation says `no-op`, says nothing was written, and explains why.
fn assert_no_op(o: &str, why_fragment: &str) {
    assert!(o.contains("no-op"), "must name itself a no-op: {o}");
    assert!(
        o.contains("nothing written"),
        "must say nothing landed: {o}"
    );
    assert!(o.contains(why_fragment), "must say why: {o}");
    // It is NOT a hard error — the tool worked, the request was vacuous.
    // `looks_like_failure` in sc-core keys on these words in the status line; see the
    // sibling test there.
    let status = o.lines().next().unwrap().to_ascii_lowercase();
    for word in ["error", "rejected", "not found", "no match", "failed"] {
        assert!(
            !status.contains(word),
            "a no-op must not read as a failure ({word:?}): {o}"
        );
    }
}

/// The `dropped_definition` guard reaches the model THROUGH `edit_file`, not just as a unit.
///
/// The verbatim edit from `rust-trait-impl`: asked to ADD `evict` and "leave the existing methods
/// behaving exactly as they do now", the model swapped `fn len` out for it. It did this four
/// times -- the trait plus all three impls -- and every caller of `len` stopped compiling. Its
/// three `evict` bodies were correct; only the deletion was wrong.
#[test]
fn edit_file_refuses_an_edit_that_deletes_a_function() {
    let ws = temp_dir("dropped-fn");
    let f = ws.join("layers.rs");
    std::fs::write(
        &f,
        "impl Store for MemStore {\n    /// How many keys are live.\n    fn len(&self) -> usize {\n        self.map.len()\n    }\n}\n",
    )
    .unwrap();
    let before = stamp(&f);

    let e = call(json!({
        "tool":"edit_file","path":"layers.rs",
        "old_str":"    /// How many keys are live.\n    fn len(&self) -> usize {\n        self.map.len()\n    }",
        "new_str":"    /// Remove the key, returning whether it was present.\n    fn evict(&mut self, key: &str) -> bool {\n        self.map.remove(key).is_some()\n    }"
    }));
    let o = obs(execute(&e, &ws));

    assert!(o.contains("rejected"), "the deletion must be refused: {o}");
    assert!(o.contains("len"), "must name the function it drops: {o}");
    assert_eq!(stamp(&f), before, "nothing may be written");
    let _ = std::fs::remove_dir_all(&ws);
}

/// The other half of the contract: ADDING a method alongside the one already there is the
/// correct move, and must sail through. A guard that blocked this would be worse than none.
#[test]
fn edit_file_allows_adding_a_function_beside_an_existing_one() {
    let ws = temp_dir("added-fn");
    let f = ws.join("layers.rs");
    std::fs::write(
        &f,
        "impl Store for MemStore {\n    fn len(&self) -> usize {\n        self.map.len()\n    }\n}\n",
    )
    .unwrap();

    let e = call(json!({
        "tool":"edit_file","path":"layers.rs",
        "old_str":"    fn len(&self) -> usize {\n        self.map.len()\n    }",
        "new_str":"    fn len(&self) -> usize {\n        self.map.len()\n    }\n\n    fn evict(&mut self, key: &str) -> bool {\n        self.map.remove(key).is_some()\n    }"
    }));
    let o = obs(execute(&e, &ws));

    assert!(!o.contains("rejected"), "adding is not deleting: {o}");
    let after = std::fs::read_to_string(&f).unwrap();
    assert!(after.contains("fn len"), "len must survive: {after}");
    assert!(after.contains("fn evict"), "evict must land: {after}");
    let _ = std::fs::remove_dir_all(&ws);
}

/// **A missed anchor that lives in ANOTHER file names that file.**
///
/// The `rust-two-stage` shape: the model copies an assertion out of the frozen `test.rs` and
/// sends it as an anchor against `lib.rs`. It occurs exactly once in test.rs and never in
/// lib.rs. The old answer was `anchor not found; closest match:` plus a block scored on shared
/// punctuation, pointing into an unrelated function -- so the model kept refining the anchor,
/// which was never the problem. 14 attempts across 18 turns.
#[test]
fn a_missed_anchor_that_lives_in_another_file_says_which_file() {
    let ws = temp_dir("sibling-anchor");
    std::fs::write(
        ws.join("lib.rs"),
        "pub fn compare(a: u32, b: u32) -> std::cmp::Ordering {\n    a.cmp(&b)\n}\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("test.rs"),
        "#[test]\nfn a_missing_component_counts_as_zero() {\n    assert_eq!(compare(&v(\"1.4\"), &v(\"1.4.0\")), Ordering::Equal);\n}\n",
    )
    .unwrap();

    let e = call(json!({
        "tool":"edit_file","path":"lib.rs",
        "old_str":"    assert_eq!(compare(&v(\"1.4\"), &v(\"1.4.0\")), Ordering::Equal);",
        "new_str":"    assert_eq!(compare(&v(\"1.4\"), &v(\"1.4.0\")), Ordering::Greater);"
    }));
    let o = obs(execute(&e, &ws));

    assert!(o.contains("test.rs"), "must name the file it IS in: {o}");
    assert!(
        o.contains("wrong file"),
        "must say plainly that the path is wrong: {o}"
    );
    assert!(
        !o.contains("closest match"),
        "must not send it hunting for a better anchor: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// DECLINE: a short anchor is in half the repository, so "it is in some other file" would be
/// noise pointing at an arbitrary one. Below the token floor it stays the ordinary miss.
#[test]
fn a_short_missed_anchor_does_not_hunt_through_other_files() {
    let ws = temp_dir("sibling-short");
    std::fs::write(ws.join("lib.rs"), "fn a() {\n    let x = 1;\n}\n").unwrap();
    std::fs::write(ws.join("other.rs"), "fn b() {\n    let y = 2;\n}\n").unwrap();

    let e = call(json!({
        "tool":"edit_file","path":"lib.rs","old_str":"let y","new_str":"let z"
    }));
    let o = obs(execute(&e, &ws));

    assert!(
        !o.contains("wrong file"),
        "a 2-token anchor must not accuse another file: {o}"
    );
    assert!(
        o.contains("anchor not found"),
        "still an ordinary miss: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// THE REGRESSION, and its severity has deliberately changed.
///
/// `old_str == new_str` used to be answered as a no-op -- but only on the paths that reach a
/// successful match. It is now REJECTED by `identical_replacement` before the file is read, so
/// the answer no longer depends on whether the anchor happened to resolve.
///
/// Why an outright rejection rather than the gentler no-op wording: the pair is wrong on its own
/// terms, exactly like `indistinct_anchor`'s bare `!`, and both of those refuse. The no-op
/// register is for a tool that WORKED on a vacuous request; this request cannot be carried out at
/// all. Measured on `rust-two-stage`, the model sent an identical pair 14 times in 18 turns.
#[test]
fn edit_file_with_an_identical_old_and_new_is_rejected_before_the_file_is_read() {
    let ws = temp_dir("noop-edit");
    let f = ws.join("a.rs");
    std::fs::write(&f, "fn f() { return 1; }\n").unwrap();
    let before = stamp(&f);

    let e = call(json!({
        "tool":"edit_file","path":"a.rs",
        "old_str":"return 1;","new_str":"return 1;"
    }));
    let o = obs(execute(&e, &ws));

    assert!(o.contains("rejected"), "must refuse the pair: {o}");
    assert!(
        o.contains("byte-identical"),
        "must name the PAIR as the defect: {o}"
    );
    assert!(
        !o.contains("1 replacement"),
        "must not claim a replacement landed: {o}"
    );
    assert_eq!(stamp(&f), before, "the file must not be rewritten at all");
    let _ = std::fs::remove_dir_all(&ws);
}

/// THE POINT OF MOVING IT EARLIER. An identical pair whose anchor does NOT resolve used to fall
/// all the way through to `anchor_not_found`, so the model was told its ANCHOR was wrong when the
/// anchor was irrelevant -- the replacement could not have changed anything wherever it landed.
/// That misdirection is what cost `rust-two-stage` eighteen turns of anchor-hunting.
#[test]
fn an_identical_pair_that_does_not_match_still_blames_the_pair_not_the_anchor() {
    let ws = temp_dir("noop-nomatch");
    let f = ws.join("lib.rs");
    std::fs::write(&f, "fn compare(a: u32, b: u32) -> bool { a == b }\n").unwrap();

    // An anchor copied out of a DIFFERENT file: it appears nowhere in lib.rs.
    let anchor = "assert_eq!(compare(&v(\"1.4\"), &v(\"1.4.0\")), Ordering::Equal);";
    let e = call(json!({
        "tool":"edit_file","path":"lib.rs","old_str":anchor,"new_str":anchor
    }));
    let o = obs(execute(&e, &ws));

    assert!(
        o.contains("byte-identical"),
        "the identical pair must be named first: {o}"
    );
    assert!(
        !o.contains("anchor not found"),
        "must NOT send the model hunting for a better anchor: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// The whole-line disambiguation path is reached only by a pair that differs, so its own
/// self-landing no-op remains a no-op. Pinned so the new pre-check cannot swallow it.
#[test]
fn edit_file_whole_line_match_that_changes_nothing_is_a_no_op() {
    let ws = temp_dir("noop-wholeline");
    let f = ws.join("m.py");
    // "return n" substring-matches twice, so this takes the whole-line branch. The pair
    // DIFFERS (trailing spaces), so it passes the identical-pair guard and reaches the
    // whole-line branch, which then trims and lands on the same text.
    std::fs::write(
        &f,
        "def is_even(n):\n    return n % 2 == 0\n\n\ndef double(n):\n    return n\n",
    )
    .unwrap();
    let before = stamp(&f);

    let e = call(json!({
        "tool":"edit_file","path":"m.py","old_str":"return n","new_str":"return n  "
    }));
    let o = obs(execute(&e, &ws));

    assert_no_op(&o, "identical");
    assert_eq!(stamp(&f), before, "the file must not be rewritten at all");
    // Pin the ROUTE, not just the outcome: this must be the whole-line branch (the only
    // producer of that phrase, write.rs), not the new identical-pair pre-check swallowing it.
    assert!(
        !o.contains("byte-identical"),
        "the pre-check must not claim this differing pair: {o}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Rewriting a file with exactly what it already holds is a no-op.
#[test]
fn write_file_with_identical_bytes_is_a_no_op() {
    let ws = temp_dir("noop-write");
    let f = ws.join("a.txt");
    std::fs::write(&f, "hello\n").unwrap();
    let before = stamp(&f);

    let w = call(json!({"tool":"write_file","path":"a.txt","content":"hello\n"}));
    let o = obs(execute(&w, &ws));

    assert_no_op(&o, "byte-for-byte");
    assert!(!o.contains(" ok "), "must not claim success: {o}");
    assert_eq!(stamp(&f), before, "the file must not be rewritten at all");
    let _ = std::fs::remove_dir_all(&ws);
}

/// Appending an empty string appends nothing.
///
/// Two layers, and this pins both. A model cannot reach the writer with `content: ""` —
/// `content` is a required non-empty `String`, so the VALIDATOR refuses the call before
/// dispatch, which is the better answer (it names the parameter). Behind it, the writer
/// itself refuses too, because `append_file` is public and a writer that answers
/// "ok (+0 bytes)" for a write that moved nothing is exactly the `edit_file` bug.
#[test]
fn append_file_with_empty_content_is_a_no_op() {
    // Layer 1: the tool surface never lets it through.
    let rejected = default_registry()
        .validate(&json!({"tool":"append_file","path":"big.css","content":""}))
        .expect_err("an empty append must not validate");
    assert!(
        rejected.to_string().contains("must not be empty"),
        "got: {rejected}"
    );

    // Layer 2: the writer itself, called directly.
    let ws = temp_dir("noop-append");
    let f = ws.join("big.css");
    std::fs::write(&f, "a {}\n").unwrap();
    let before = stamp(&f);

    let o = crate::builtin::write::append_file(&ws, "big.css", "");
    assert_no_op(&o, "empty");
    assert!(
        !o.contains("+0 bytes"),
        "must not report a 0-byte success: {o}"
    );
    assert_eq!(stamp(&f), before, "the file must not be touched at all");

    // ...and it must not conjure an empty file at a path that does not exist.
    let o = crate::builtin::write::append_file(&ws, "new.css", "");
    assert_no_op(&o, "empty");
    assert!(
        !ws.join("new.css").exists(),
        "an empty append must not conjure a file"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Replacing a line range with exactly the text already there is a no-op.
#[test]
fn edit_lines_replacing_a_range_with_itself_is_a_no_op() {
    let ws = temp_dir("noop-lines");
    let f = ws.join("a.rs");
    std::fs::write(&f, "fn f() {\n    let x = 1;\n}\n").unwrap();
    let before = stamp(&f);

    let e = call(json!({
        "tool":"edit_lines","path":"a.rs","start":2,"end":2,"new_text":"    let x = 1;"
    }));
    let o = obs(execute(&e, &ws));

    assert_no_op(&o, "identical");
    assert!(!o.contains("replaced lines"), "must not claim an edit: {o}");
    assert_eq!(stamp(&f), before, "the file must not be rewritten at all");
    let _ = std::fs::remove_dir_all(&ws);
}

/// Replacing a function with the body it already has is a no-op.
#[test]
fn edit_function_with_an_identical_body_is_a_no_op() {
    let ws = temp_dir("noop-efn");
    let f = ws.join("m.rs");
    let body = "fn pick(r: u32) -> u32 {\n    r + 1\n}";
    std::fs::write(&f, format!("{body}\n")).unwrap();
    let before = stamp(&f);

    let e = call(json!({
        "tool":"edit_function","path":"m.rs","name":"pick","new_body":body
    }));
    let o = obs(execute(&e, &ws));

    assert_no_op(&o, "identical");
    assert!(!o.contains("replaced lines"), "must not claim an edit: {o}");
    assert_eq!(stamp(&f), before, "the file must not be rewritten at all");
    let _ = std::fs::remove_dir_all(&ws);
}

/// `create_file` has no no-op case: it refuses an existing path, so the only write it
/// performs creates a file that was not there — always a change, empty content included.
#[test]
fn create_file_has_no_no_op_case() {
    let ws = temp_dir("noop-create");
    // Creating a file that was not there is always a workspace change...
    let c = call(json!({"tool":"create_file","path":"e.txt","content":"x"}));
    let o = obs(execute(&c, &ws));
    assert!(o.contains("ok (1 bytes)"), "got: {o}");
    assert!(ws.join("e.txt").exists());
    // ...and a second create with the SAME content is refused, not silently "ok".
    // That refusal, not a no-op observation, is the answer for the identical-bytes case.
    let again = obs(execute(&c, &ws));
    assert!(again.contains("already exists"), "got: {again}");
    // Empty content can't even be asked for: the validator rejects it.
    assert!(default_registry()
        .validate(&json!({"tool":"create_file","path":"z.txt","content":""}))
        .is_err());
    let _ = std::fs::remove_dir_all(&ws);
}

/// **THE HAPPY PATH IS PINNED.** The no-op guard must not touch the wording of a real
/// edit — the messages below are the exact bytes each writer produced before the fix,
/// and several places (the UI, `looks_like_failure`, the batched-write note) key on them.
#[test]
fn a_real_write_still_reports_exactly_as_before() {
    let ws = temp_dir("noop-happy");

    std::fs::write(ws.join("a.rs"), "fn f() { return 1; }\n").unwrap();
    assert_eq!(
        obs(execute(
            &call(json!({
                "tool":"edit_file","path":"a.rs","old_str":"return 1;","new_str":"return 2;"
            })),
            &ws
        )),
        "edit_file a.rs ok (1 replacement)"
    );

    assert_eq!(
        obs(execute(
            &call(json!({"tool":"write_file","path":"w.txt","content":"hello\n"})),
            &ws
        )),
        "write_file w.txt ok (6 bytes)"
    );

    assert_eq!(
        obs(execute(
            &call(json!({"tool":"create_file","path":"c.txt","content":"hi"})),
            &ws
        )),
        "create_file c.txt ok (2 bytes)"
    );

    assert_eq!(
        obs(execute(
            &call(json!({"tool":"append_file","path":"w.txt","content":"more\n"})),
            &ws
        )),
        "append_file w.txt ok (+5 bytes, 11 total)"
    );

    std::fs::write(ws.join("l.rs"), "fn f() {\n    let x = 1;\n}\n").unwrap();
    assert_eq!(
        obs(execute(
            &call(json!({
                "tool":"edit_lines","path":"l.rs","start":2,"end":2,"new_text":"    let x = 2;"
            })),
            &ws
        )),
        "edit_lines l.rs ok (replaced lines 2..=2; file now 3 lines)"
    );

    std::fs::write(ws.join("m.rs"), "fn pick(r: u32) -> u32 {\n    r + 1\n}\n").unwrap();
    assert_eq!(
        obs(execute(
            &call(json!({
                "tool":"edit_function","path":"m.rs","name":"pick",
                "new_body":"fn pick(r: u32) -> u32 {\n    r + 2\n}"
            })),
            &ws
        )),
        "edit_function m.rs:pick ok (replaced lines 1..=3; file now 3 lines)"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// The indentation-tolerant SPAN rung: a single-line anchor naming a
// sub-expression, whose only mismatch is leading whitespace.
// ---------------------------------------------------------------------------

/// The `Window::values` fixture from `evals/ladder/tasks/rust-symptomatic`, trimmed to
/// the part the live failure was measured against.
fn ring_buffer_fixture() -> &'static str {
    "\
//! A fixed-capacity ring buffer of readings, with a rolling mean.

pub struct Window {
    buf: Vec<i64>,
    cap: usize,
    head: usize,
    len: usize,
    sum: i64,
}

impl Window {
    /// The readings, oldest first.
    pub fn values(&self) -> Vec<i64> {
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            out.push(self.buf[(self.head + i) % self.cap]);
        }
        out
    }
}
"
}

/// **THE LIVE CASE.** The model anchors on the sub-expression with 8 spaces of
/// indentation; the file line carries 12 and wraps it in `out.push(` … `);`.
///
/// Exact occurrences: 0. Trimmed occurrences: exactly 1. The anchor is unambiguous, so
/// the edit must land — and it must replace only the expression, leaving `out.push(`
/// and `);` intact. 24 of 43 anchor failures across the Mellum transcripts were this.
#[test]
fn edit_file_partial_line_anchor_with_wrong_indent_lands() {
    let ws = temp_dir("edit-span-live");
    std::fs::write(ws.join("lib.rs"), ring_buffer_fixture()).unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "lib.rs",
        // Eight spaces, and only the indexing expression — not the whole statement.
        "old_str": "        self.buf[(self.head + i) % self.cap]",
        "new_str": "        self.buf[(self.head + i) % self.len]",
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("ok (1 replacement, matched ignoring indentation)"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("lib.rs")).unwrap(),
        "\
//! A fixed-capacity ring buffer of readings, with a rolling mean.

pub struct Window {
    buf: Vec<i64>,
    cap: usize,
    head: usize,
    len: usize,
    sum: i64,
}

impl Window {
    /// The readings, oldest first.
    pub fn values(&self) -> Vec<i64> {
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            out.push(self.buf[(self.head + i) % self.len]);
        }
        out
    }
}
",
        "only the span moved: `out.push(` and `);` survive, and so does the 12-space indent"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **Ambiguity is rejected, never guessed.** The same trimmed anchor in two places
/// means we cannot know which the model meant, so nothing is written.
#[test]
fn edit_file_partial_line_anchor_must_be_unique() {
    let ws = temp_dir("edit-span-amb");
    let src = "\
fn f() {
    let a = compute(self.buf[i]);
        let b = compute(self.buf[i]);
}
";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "a.rs",
        "old_str": "  self.buf[i]",
        "new_str": "  self.buf[j]",
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("anchor not found") || o.contains("ambiguous"),
        "got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        src,
        "two candidates: the file is untouched"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A mid-line anchor replaces ONLY its span — not the line it sits in. This is what
/// separates this rung from the whole-line disambiguation above it.
#[test]
fn edit_file_partial_line_anchor_replaces_only_the_span() {
    let ws = temp_dir("edit-span-midline");
    std::fs::write(
        ws.join("a.rs"),
        "fn f() {\n    let x = wrap(inner(1), tail);\n}\n",
    )
    .unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "a.rs",
        // Indented anchor naming a sub-expression in the middle of the line.
        "old_str": "        inner(1)",
        "new_str": "        inner(2)",
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("matched ignoring indentation"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n    let x = wrap(inner(2), tail);\n}\n",
        "`let x = wrap(` and `, tail);` are untouched"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A MULTI-LINE anchor never reaches this rung, even when its trimmed form would occur
/// once. Multi-line blocks are `fuzzy_line_block_replace`'s job, and it re-indents each
/// line rather than splicing a raw span.
#[test]
fn edit_file_multi_line_anchor_still_takes_the_block_path() {
    let ws = temp_dir("edit-span-multi");
    std::fs::write(
        ws.join("a.rs"),
        "fn f() {\n    let a = 1;\n    let b = 2;\n}\n",
    )
    .unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "a.rs",
        // Two lines, both under-indented: the block path matches on line signatures.
        "old_str": "let a = 1;\nlet b = 2;",
        "new_str": "let a = 10;\nlet b = 20;",
    }));
    let o = obs(execute(&e, &ws));
    assert!(
        o.contains("whitespace-tolerant match"),
        "the block rung must answer, not the span rung; got: {o}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        "fn f() {\n    let a = 10;\n    let b = 20;\n}\n",
        "each line re-indented to the file's own indent"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **The guards still fire on this path.** A partial-line anchor is not a way around
/// `destructive_replacement`: replacing real code with bare punctuation is rejected
/// here exactly as it is on the exact-match path.
#[test]
fn edit_file_partial_line_anchor_still_hits_the_destructive_guard() {
    let ws = temp_dir("edit-span-destructive");
    let src = "fn f() {\n    let x = compute(alpha_beta_gamma);\n}\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "a.rs",
        "old_str": "        compute(alpha_beta_gamma)",
        "new_str": "        :",
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("rejected"), "got: {o}");
    assert!(o.contains("DESTROY the line"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        src,
        "nothing written"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// …and the brace-balance tripwire too: a span replacement that unbalances the file is
/// rejected before it reaches disk. This guard is gated on `count == 1` further down, so
/// the new rung has to run it itself.
#[test]
fn edit_file_partial_line_anchor_still_hits_the_delimiter_guard() {
    let ws = temp_dir("edit-span-delims");
    let src = "fn f() {\n    let x = wrap(inner(1), tail);\n}\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "a.rs",
        // Drops a closing paren: the file would no longer balance.
        "old_str": "        inner(1)",
        "new_str": "        inner(1",
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("rejected"), "got: {o}");
    assert!(o.contains("unbalanced the file's delimiters"), "got: {o}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.rs")).unwrap(),
        src,
        "nothing written"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Zero occurrences of the trimmed anchor still reports the missed anchor, unchanged.
#[test]
fn edit_file_absent_anchor_still_reports_not_found() {
    let ws = temp_dir("edit-span-absent");
    let src = "fn f() {\n    let x = 1;\n}\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let e = call(json!({
        "tool": "edit_file",
        "path": "a.rs",
        "old_str": "        nothing_like_this(9)",
        "new_str": "        something(9)",
    }));
    let o = obs(execute(&e, &ws));
    assert!(o.contains("anchor not found"), "got: {o}");
    assert_eq!(std::fs::read_to_string(ws.join("a.rs")).unwrap(), src);
    let _ = std::fs::remove_dir_all(&ws);
}
