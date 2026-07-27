//! Strategy tests: extraction, the salvage paths, and capability selection.

use super::error::*;
use super::kinds::*;
use super::repair::*;
use sc_model::{OutputConstraint, ToolCalling};
use sc_tools::default_registry;

#[test]
fn extracts_a_clean_call() {
    let reg = default_registry();
    let call = ParseRepair
        .extract(r#"{"tool":"read_file","path":"a.txt"}"#, &reg)
        .unwrap();
    assert_eq!(call.name, "read_file");
    assert_eq!(call.str("path"), Some("a.txt"));
}

#[test]
fn prefers_the_real_call_over_a_narrated_illustration() {
    // Observed live (2026-07-15): the coder model NARRATES a tool call in prose as an
    // illustration ("Let me edit: {"tool":"edit_file",...truncated...}") and THEN emits the
    // real complete call. The narrated copy is often incomplete/mangled; picking it applies
    // garbage. The complete, well-formed call must win.
    let reg = default_registry();
    let raw = "Let me make the edit: {\"tool\":\"edit_file\",\"path\":\"terrain.rs\",\
               \"old_str\":\"pub struct Terrain { seed\
               \n\nActually, here is the real edit:\n\
               {\"tool\":\"edit_file\",\"path\":\"terrain.rs\",\"old_str\":\"let x = 1;\",\
               \"new_str\":\"let x = 2;\"}";
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.name, "edit_file");
    assert_eq!(
        call.str("old_str"),
        Some("let x = 1;"),
        "picked the complete call"
    );
    assert_eq!(call.str("new_str"), Some("let x = 2;"));
}

#[test]
fn rejects_a_run_on_edit_that_absorbed_the_next_argument() {
    // The ship_render.rs corruption (observed live 2026-07-21): the model's `new_str` value ran
    // past its closing quote and absorbed a following `,"old_str":"…` — the braces still
    // balanced, so it parsed as ONE object with a `new_str` containing raw JSON. Applying it
    // spliced `};","old_str":"use …` into the source. It MUST be rejected, not written.
    let reg = default_registry();
    // The `new_str` VALUE literally contains the run-on marker `","old_str":` — the model's
    // broken quoting embedded the next key inside the string (an escaped inner quote), so it
    // parses as one object with a corrupt `new_str`. This is what landed raw JSON in the file.
    let raw = r#"{"tool":"edit_file","path":"ship_render.rs","old_str":"use foo::{Bar};","new_str":"use foo::{Bar};\n\nuse foo::SeatType;\",\"old_str\":\"use foo::{Bar};"}"#;
    let err = ParseRepair.extract(raw, &reg).unwrap_err();
    assert_eq!(
        err,
        RepairError::Swallowed,
        "run-on edit rejected, not applied"
    );
}

#[test]
fn does_not_reject_a_legit_edit_whose_code_mentions_old_str_as_text() {
    // Guard against a false positive: real code can contain the identifier `old_str` — only a
    // value that RUNS ON into a JSON `","old_str":` framing is corrupt. A clean edit whose body
    // merely mentions the word must still apply.
    let reg = default_registry();
    let raw = r#"{"tool":"edit_file","path":"a.rs","old_str":"let old_str = 1;","new_str":"let old_str = 2; // renamed later"}"#;
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.name, "edit_file");
    assert_eq!(
        call.str("new_str"),
        Some("let old_str = 2; // renamed later")
    );
}

#[test]
fn skips_a_swallowed_call_when_a_clean_one_also_parsed() {
    // Both a swallowed call (its old_str embeds another "tool":) and a clean read parsed.
    // The clean one must win, not the corrupt swallowed edit.
    let reg = default_registry();
    let raw = "{\"tool\":\"edit_file\",\"path\":\"a.rs\",\"old_str\":\"x {\\\"tool\\\":\\\"y\\\"}\",\"new_str\":\"z\"}\
               <tool_call|>{\"tool\":\"read_file\",\"path\":\"a.rs\"}";
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(
        call.name, "read_file",
        "skipped the swallowed edit for the clean read"
    );
}

#[test]
fn batched_turn_runs_the_first_progress_call_not_the_leading_read() {
    // Gemma-4 emits several calls in one turn separated by `<tool_call|>`. The loop
    // runs one action/turn, so we must pick the call that makes PROGRESS (create),
    // not the leading re-read the model already has. Observed live 2026-06-24.
    let reg = default_registry();
    let raw = "{\"tool\":\"read_file\",\"path\":\"test_app.py\"}<tool_call|>\
               {\"tool\":\"create_file\",\"path\":\"app.py\",\"content\":\"x = 1\"}<tool_call|>\
               {\"tool\":\"run_verification\"}<tool_call|>{\"tool\":\"finish\"}";
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(
        call.name, "create_file",
        "must skip the leading read to the create"
    );
    assert_eq!(call.str("path"), Some("app.py"));
}

#[test]
fn batched_reads_only_returns_the_first() {
    // If every batched call is a no-op read, just take the first (no progress call).
    let reg = default_registry();
    let raw = "{\"tool\":\"read_file\",\"path\":\"a.py\"}<tool_call|>\
               {\"tool\":\"read_file\",\"path\":\"b.py\"}";
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.name, "read_file");
    assert_eq!(call.str("path"), Some("a.py"));
}

#[test]
fn write_batch_collects_consecutive_distinct_path_writes() {
    // The thread-3 case: the model emits the whole app as create/write calls in one turn.
    // extract_write_batch returns the leading run of DISTINCT-path whole-file writes.
    let reg = default_registry();
    let raw = "{\"tool\":\"create_file\",\"path\":\"store.py\",\"content\":\"a\"}\
               {\"tool\":\"create_file\",\"path\":\"app.py\",\"content\":\"b\"}\
               {\"tool\":\"write_file\",\"path\":\"util.py\",\"content\":\"c\"}\
               {\"tool\":\"run_verification\"}";
    let batch = extract_write_batch(raw, &reg);
    let paths: Vec<_> = batch.iter().filter_map(|c| c.str("path")).collect();
    assert_eq!(
        paths,
        vec!["store.py", "app.py", "util.py"],
        "stops at run_verification"
    );
}

#[test]
fn write_batch_stops_at_an_edit_or_a_repeated_path() {
    let reg = default_registry();
    // Gate 1: an edit_file (anchored — needs current file state) ends the batch.
    let raw_edit = "{\"tool\":\"write_file\",\"path\":\"a.py\",\"content\":\"x\"}\
                    {\"tool\":\"edit_file\",\"path\":\"a.py\",\"old_str\":\"x\",\"new_str\":\"y\"}";
    let b1 = extract_write_batch(raw_edit, &reg);
    assert_eq!(b1.len(), 1, "edit ends the batch: {b1:?}");
    // Gate 2: a re-write of a path already in the batch (revision — react first) ends it.
    let raw_dup = "{\"tool\":\"write_file\",\"path\":\"a.py\",\"content\":\"x\"}\
                   {\"tool\":\"write_file\",\"path\":\"a.py\",\"content\":\"x2\"}";
    let b2 = extract_write_batch(raw_dup, &reg);
    assert_eq!(b2.len(), 1, "duplicate path ends the batch: {b2:?}");
}

#[test]
fn write_batch_is_empty_when_the_turn_does_not_lead_with_a_write() {
    let reg = default_registry();
    // A leading read → no batch (the loop's normal single-action path handles it).
    let raw = "{\"tool\":\"read_file\",\"path\":\"a.py\"}\
               {\"tool\":\"write_file\",\"path\":\"b.py\",\"content\":\"x\"}";
    assert!(extract_write_batch(raw, &reg).is_empty());
}

#[test]
fn tolerates_prose_and_braces_in_strings() {
    let reg = default_registry();
    let raw = "Sure:\n{\"tool\":\"write_file\",\"path\":\"x\",\"content\":\"a { b } c\"}\ndone";
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.str("content"), Some("a { b } c"));
}

#[test]
fn tolerates_raw_newlines_inside_a_string_value() {
    // A coder model writes a multi-line `old_str` with LITERAL newlines (and even a
    // mix of escaped + raw, exactly as qwen3-coder-30b did 2026-06-23). Strict JSON
    // forbids raw control chars in strings; the sanitizer must rescue it.
    let reg = default_registry();
    let raw = "{\"tool\":\"edit_file\",\"path\":\"app.py\",\"old_str\":\"def page():\n    start = n * 3\",\"new_str\":\"def page():\n    start = (n-1) * 3\"}";
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.name, "edit_file");
    assert_eq!(call.str("old_str"), Some("def page():\n    start = n * 3"));
    assert_eq!(
        call.str("new_str"),
        Some("def page():\n    start = (n-1) * 3")
    );
}

#[test]
fn edit_file_with_multiline_unescaped_bodies_is_recovered() {
    // The single largest live parse-failure class: edit_file whose old_str/new_str carry a
    // multi-line code snippet with RAW newlines (and inner quotes), which is invalid JSON.
    // strict parse + the control-char escaper both fail (an inner `"` desyncs them); the
    // key-aware edit repair pulls path/old_str/new_str out by position instead.
    let reg = default_registry();
    // Shaped like the real live failures: multi-line bodies with raw newlines (the invalid-
    // JSON part). Single-quoted Python so the boundary detection isn't fighting an inner `"`
    // right at the separator (that pathological case is rare and left to strict parsing).
    let raw = "{\"tool\":\"edit_file\",\"path\":\"app.py\",\"old_str\":\"def f():\n    return 1\n\",\"new_str\":\"def f():\n    return 2\n\"}";
    let call = ParseRepair.extract(raw, &reg).expect("recovers the edit");
    assert_eq!(call.name, "edit_file");
    assert_eq!(call.str("path"), Some("app.py"));
    // Both bodies recovered with their real newlines, split at the right boundary.
    assert!(
        call.str("old_str").unwrap().contains("return 1"),
        "old: {:?}",
        call.str("old_str")
    );
    assert!(
        call.str("new_str").unwrap().contains("return 2"),
        "new: {:?}",
        call.str("new_str")
    );
    assert!(call.str("old_str").unwrap().contains('\n'));
}

#[test]
fn incidental_python_dicts_in_prose_are_not_mistaken_for_a_tool_call() {
    // The model "thinks out loud" with Python dicts in the prose (`{'n': 5}`). The
    // extractor must IGNORE those (no "tool" key) and not try to parse one as the call.
    let reg = default_registry();
    // Pure prose with dicts, no tool call → a clean repairable error (NoJson), not a
    // confusing "key must be a string".
    let prose = "I'll return {'result': 25} when given {'n': 5}. Let me implement it.";
    assert!(ParseRepair.extract(prose, &reg).is_err());
    // Prose dicts FOLLOWED by a real tool call → the real call is found.
    let mixed = "First {'n': 5} then the call:\n{\"tool\":\"finish\"}";
    assert_eq!(ParseRepair.extract(mixed, &reg).unwrap().name, "finish");
}

#[test]
fn a_fenced_code_block_recovers_a_write_to_the_focused_file() {
    // The model replies with a ```python``` block instead of a JSON tool call. With a known
    // focus file, extract_markdown_write synthesizes the write_file it meant.
    let reg = default_registry();
    let raw = "Here is the implementation:\n\n```python\ndef square(n):\n    return n * n\n```\n";
    let call = extract_markdown_write(raw, "mathlib.py", &reg).expect("recovered a write");
    assert_eq!(call.name, "write_file");
    assert_eq!(call.str("path"), Some("mathlib.py"));
    assert_eq!(
        call.str("content"),
        Some("def square(n):\n    return n * n\n")
    );
    // No fence → no recovery (don't invent a write from prose).
    assert!(extract_markdown_write("just prose, no code", "x.py", &reg).is_none());
}

#[test]
fn write_file_with_a_literal_python_docstring_is_recovered() {
    // The writefile-docstring-json-break: a model writes a real Python `"""docstring"""`
    // inside `content`, whose inner `"` closes the JSON string early so strict parsing
    // fails and the file is never written. The key-aware fallback must recover it.
    let reg = default_registry();
    let raw = "{\"tool\":\"write_file\",\"path\":\"app.py\",\"content\":\"def f():\n    \"\"\"doc string\"\"\"\n    return 1\n\"}";
    let call = ParseRepair
        .extract(raw, &reg)
        .expect("the docstring write_file must be recovered, not dropped");
    assert_eq!(call.name, "write_file");
    assert_eq!(call.str("path"), Some("app.py"));
    // The literal body (triple quotes intact) is preserved.
    let content = call.str("content").unwrap();
    assert!(
        content.contains("\"\"\"doc string\"\"\""),
        "got: {content:?}"
    );
    assert!(content.contains("def f():") && content.contains("return 1"));
}

#[test]
fn truncated_write_file_is_salvaged_to_the_partial_body() {
    // The css-truncation loop: a small model's write_file content runs past its output
    // length and the reply is cut off mid-string — no closing quote, JSON never parses,
    // and both the strict path and the closed-quote repair fail. The salvage must land the
    // partial content that DID arrive (rebuilt as write_file) so the model can append the
    // rest, instead of re-emitting the same over-long content forever.
    let reg = default_registry();
    let raw = "{\"tool\":\"write_file\",\"path\":\"styles.css\",\"content\":\"body {\\n  color: #333;\\n}\\n\\n#home {\\n  padding: 4rem";
    let call = ParseRepair
        .extract(raw, &reg)
        .expect("a truncated write_file must be salvaged, not looped");
    assert_eq!(call.name, "write_file");
    assert_eq!(call.str("path"), Some("styles.css"));
    let content = call.str("content").unwrap();
    // The head that arrived is preserved with real newlines applied.
    assert!(
        content.starts_with("body {\n  color: #333;\n}"),
        "got: {content:?}"
    );
    assert!(
        content.contains("#home {\n  padding: 4rem"),
        "got: {content:?}"
    );
}

#[test]
fn truncated_append_file_stays_append_not_write() {
    // A truncated append_file must be salvaged as append_file (additive — the partial chunk
    // is safe to add and the model continues), NOT rewritten as write_file (which would
    // clobber everything appended so far). This is the site2 gap: append chunks truncated
    // and had no salvage, dropping the #cta rule and leaving a dangling <span>.
    let reg = default_registry();
    let raw = "{\"tool\":\"append_file\",\"path\":\"styles.css\",\"content\":\"#cta {\\n  padding: 15px;\\n}\\n\\n#menu li {\\n  display: flex";
    let call = ParseRepair
        .extract(raw, &reg)
        .expect("a truncated append_file must be salvaged");
    assert_eq!(
        call.name, "append_file",
        "append semantics preserved, not collapsed to write"
    );
    assert_eq!(call.str("path"), Some("styles.css"));
    assert!(call
        .str("content")
        .unwrap()
        .starts_with("#cta {\n  padding: 15px;"));
}

#[test]
fn truncated_write_tolerates_equals_for_colon_separator() {
    // Observed live on an append turn: the model emitted `"content"=` instead of `"content":`.
    // Combined with truncation, strict parsing fails at the `=`; the salvage accepts either
    // separator so the partial body still lands.
    let reg = default_registry();
    let raw = "{\"tool\":\"append_file\",\"path\":\"a.html\",\"content\"=\"  <li>Latte</li>\\n  <li>Mocha";
    let call = ParseRepair
        .extract(raw, &reg)
        .expect("the `=` separator variant must still be salvaged");
    assert_eq!(call.name, "append_file");
    assert!(call.str("content").unwrap().contains("<li>Latte</li>"));
}

#[test]
fn truncation_salvage_does_not_fire_when_content_is_properly_closed() {
    // A complete, well-formed write_file must NOT be treated as truncated — it parses
    // strictly and the salvage never runs. Byte-exact content proves no interference.
    let reg = default_registry();
    let raw = r#"{"tool":"write_file","path":"a.css","content":"body { color: red; }\n"}"#;
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.str("content"), Some("body { color: red; }\n"));
}

#[test]
fn truncation_salvage_ignores_an_empty_partial_body() {
    // Cut off right at the opening quote — nothing meaningful arrived. Don't write an
    // empty file; fall through to the normal error so the model retries cleanly.
    let raw = "{\"tool\":\"write_file\",\"path\":\"a.css\",\"content\":\"";
    assert!(repair_truncated_file_write(raw).is_none());
}

#[test]
fn recovery_handles_content_whose_body_contains_braces() {
    // Code content with `{` / `}` (a dict) AND an inner quote — the balanced-brace object
    // scan can mis-cut here, so recovery must still pull the right body by key position.
    let reg = default_registry();
    let raw = "{\"tool\":\"create_file\",\"path\":\"d.py\",\"content\":\"X = {\"a\": 1}\nY = \"\"\"q\"\"\"\n\"}";
    let call = ParseRepair.extract(raw, &reg).expect("recovered");
    assert_eq!(call.name, "create_file");
    let content = call.str("content").unwrap();
    assert!(content.contains("X = {\"a\": 1}"), "got: {content:?}");
    assert!(content.contains("\"\"\"q\"\"\""));
}

#[test]
fn recovery_does_not_fire_on_a_well_formed_call() {
    // A normal, parseable write_file must take the strict path and be byte-exact — the
    // fallback only runs when strict parsing fails, so this proves no regression.
    let reg = default_registry();
    let raw = r#"{"tool":"write_file","path":"a.py","content":"x = 1\ny = 2\n"}"#;
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.str("content"), Some("x = 1\ny = 2\n"));
}

#[test]
fn already_escaped_newlines_still_parse_unchanged() {
    // The sanitizer must not double-escape a correctly-escaped `\n`.
    let reg = default_registry();
    let raw = r#"{"tool":"write_file","path":"a.py","content":"x = 1\ny = 2"}"#;
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.str("content"), Some("x = 1\ny = 2"));
}

#[test]
fn no_json_is_a_distinct_repairable_error() {
    let reg = default_registry();
    let err = ParseRepair.extract("no json here", &reg).unwrap_err();
    assert_eq!(err, RepairError::NoJson);
    let prompt = err.repair_prompt();
    // The repair shows the model a concrete valid example, not just "wrong".
    assert!(prompt.contains("\"tool\""), "{prompt}");
    assert!(prompt.contains("read_file"), "{prompt}");
}

#[test]
fn schema_violation_surfaces_the_precise_reason() {
    let reg = default_registry();
    // valid JSON, wrong shape: read_file needs a path
    let err = ParseRepair
        .extract(r#"{"tool":"read_file"}"#, &reg)
        .unwrap_err();
    match &err {
        RepairError::Invalid(_) => {}
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert!(err.repair_prompt().contains("requires parameter"), "{err}");
}

#[test]
fn unknown_tool_repair_lists_the_real_tools() {
    let reg = default_registry();
    let err = ParseRepair
        .extract(r#"{"tool":"delete_everything"}"#, &reg)
        .unwrap_err();
    let prompt = err.repair_prompt();
    assert!(prompt.contains("read_file"), "{prompt}");
}

#[test]
fn preamble_lists_every_tool() {
    let reg = default_registry();
    let preamble = ParseRepair.system_preamble(&reg);
    for spec in reg.specs() {
        assert!(
            preamble.contains(spec.name),
            "missing {} in preamble",
            spec.name
        );
    }
}

#[test]
fn native_strategy_attaches_a_tools_constraint() {
    let reg = default_registry();
    let mut req = sc_model::GenerateRequest::new(vec![]);
    NativeTools.prepare_request(&mut req, &reg);
    match req.constraint {
        Some(OutputConstraint::Tools(ref tools)) => {
            assert_eq!(tools.len(), reg.specs().len());
            assert!(tools.iter().any(|t| t.name == "read_file"));
        }
        other => panic!("expected Tools constraint, got {other:?}"),
    }
}

#[test]
fn grammar_strategy_attaches_a_grammar_constraint() {
    let reg = default_registry();
    let mut req = sc_model::GenerateRequest::new(vec![]);
    Grammar.prepare_request(&mut req, &reg);
    match req.constraint {
        Some(OutputConstraint::Grammar(ref g)) => assert!(g.contains("root ::=")),
        other => panic!("expected Grammar constraint, got {other:?}"),
    }
}

#[test]
fn all_strategies_share_the_same_validating_extractor() {
    // Whatever the strategy, a valid tool-call string validates and a bad one
    // is a repairable error — extraction is uniform across strategies.
    let reg = default_registry();
    let good = r#"{"tool":"finish"}"#;
    let bad = r#"{"tool":"nope"}"#;
    for s in [
        &ParseRepair as &dyn ToolCallStrategy,
        &NativeTools,
        &Grammar,
    ] {
        assert!(s.extract(good, &reg).is_ok(), "{} rejected good", s.name());
        assert!(s.extract(bad, &reg).is_err(), "{} accepted bad", s.name());
    }
}

#[test]
fn select_strategy_follows_capabilities() {
    use sc_model::Capabilities;
    let caps = |tc| Capabilities {
        max_context_tokens: 8192,
        tool_calling: tc,
        on_device: false,
    };
    assert_eq!(
        select_strategy(&caps(ToolCalling::None)).name(),
        "parse-repair"
    );
    assert_eq!(
        select_strategy(&caps(ToolCalling::OpenAiStyle)).name(),
        "native-fc"
    );
    assert_eq!(select_strategy(&caps(ToolCalling::Gbnf)).name(), "gbnf");
}
