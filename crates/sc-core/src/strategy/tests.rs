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

/// **The scratchpad grammar is an experiment, and the strict grammar is not it.**
///
/// Spec 02 says constrain the envelope and let the model reason first; the investigate
/// path found unconstrained reasoning ran to the token cap and went strict. `with_scratchpad`
/// is the bounded middle, opt in. The plain `Grammar` must keep sending the strict grammar --
/// that is the arm with a measurement behind it.
#[test]
fn plain_grammar_stays_strict_and_with_scratchpad_bounds_a_prefix() {
    let reg = sc_tools::read_only_registry();
    let mut strict = sc_model::GenerateRequest::new(vec![]);
    Grammar.prepare_request(&mut strict, &reg);
    let Some(OutputConstraint::Grammar(strict_g)) = strict.constraint else {
        panic!("expected a grammar constraint");
    };
    assert_eq!(
        strict_g,
        sc_tools::registry_gbnf(&reg),
        "the strict arm is untouched"
    );
    assert!(
        !strict_g.contains("scratch"),
        "no prose in the strict grammar"
    );
    assert_eq!(Grammar.name(), "gbnf");

    let mut req = sc_model::GenerateRequest::new(vec![]);
    let scratch = Grammar::with_scratchpad(400);
    scratch.prepare_request(&mut req, &reg);
    let Some(OutputConstraint::Grammar(g)) = req.constraint else {
        panic!("expected a grammar constraint");
    };
    assert_eq!(g, sc_tools::registry_gbnf_with_scratchpad(&reg, 400));
    assert!(g.starts_with("root ::= think? scratch? call\n"), "{g}");
    assert!(
        g.contains("[^{]{0,400}"),
        "the bound rides in the grammar: {g}"
    );
    assert_eq!(
        scratch.name(),
        "gbnf+scratchpad",
        "the arms are distinguishable in logs"
    );
    // The preamble tells the model it may think, and the one rule the grammar imposes.
    let preamble = scratch.system_preamble(&reg);
    assert!(preamble.contains("400 characters"), "{preamble}");
    assert!(
        preamble.contains("read_file"),
        "still lists the tools: {preamble}"
    );
}

/// What the scratchpad grammar can produce, extracted: prose then the object, a think
/// block then the object, and the degenerate case of no prefix at all.
#[test]
fn scratchpad_extract_skips_the_prefix_and_validates_the_call() {
    let reg = sc_tools::read_only_registry();
    let s = Grammar::with_scratchpad(400);
    let call = s
        .extract(
            "some thinking\n{\"tool\":\"finish\",\"summary\":\"done\"}",
            &reg,
        )
        .expect("prose before the object is the whole point");
    assert_eq!(call.name, "finish");
    assert_eq!(call.str("summary"), Some("done"));

    // A think block, and one that narrates a call in braces -- the grammar allows `{`
    // inside <think>, so the narrated call must not be mistaken for the real one.
    let raw = "<think>I could search: {\"tool\":\"search_code\",\"query\":\"x\"} but no</think>\n\
               {\"tool\":\"read_file\",\"path\":\"a.rs\"}";
    let call = s
        .extract(raw, &reg)
        .expect("the call after the think block");
    assert_eq!(
        call.name, "read_file",
        "narration inside <think> is not the call"
    );
    assert_eq!(call.str("path"), Some("a.rs"));

    // No prefix: exactly what the strict grammar would have emitted.
    let call = s
        .extract("{\"tool\":\"list_dir\",\"path\":\".\"}", &reg)
        .unwrap();
    assert_eq!(call.name, "list_dir");
}

/// Prose alone is the same failure ParseRepair reports -- the scratchpad path adds no
/// error vocabulary of its own, so the loop's repair prompt is unchanged.
#[test]
fn scratchpad_extract_fails_like_parse_repair_on_prose_alone() {
    let reg = sc_tools::read_only_registry();
    let prose = "I think the answer is in ship_render.rs\n";
    let scratch_err = Grammar::with_scratchpad(400)
        .extract(prose, &reg)
        .unwrap_err();
    let plain_err = ParseRepair.extract(prose, &reg).unwrap_err();
    assert_eq!(scratch_err, plain_err);
    assert_eq!(scratch_err, RepairError::NoJson);
    // An unterminated think block is not silently swallowed either.
    let err = Grammar::with_scratchpad(400)
        .extract("<think>never closed", &reg)
        .unwrap_err();
    assert_eq!(err, RepairError::NoJson);
}

/// (a) The reported defect: a trailing extra `}`. The call must STILL parse to the right
/// thing — the recovery is what makes those 84 turns work — AND the reply must be flagged.
#[test]
fn a_trailing_brace_still_parses_and_is_flagged() {
    let reg = default_registry();
    // The exact shape from the run logs (keys in the order the model emitted them).
    let raw =
        r#"{"new_str":"let x = 2;","old_str":"let x = 1;","path":"lib.rs","tool":"edit_file"}}"#;

    // Recovery is UNCHANGED: the call is still extracted, correctly, in full.
    let call = ParseRepair
        .extract(raw, &reg)
        .expect("the trailing brace must not cost the turn");
    assert_eq!(call.name, "edit_file");
    assert_eq!(call.str("path"), Some("lib.rs"));
    assert_eq!(call.str("old_str"), Some("let x = 1;"));
    assert_eq!(call.str("new_str"), Some("let x = 2;"));

    // ...but it is no longer SILENT.
    assert!(
        has_malformed_json_envelope(raw),
        "the trailing `}}` must raise the fault"
    );
    // Proof the reply really was invalid JSON — i.e. the fault is telling the truth.
    assert!(
        serde_json::from_str::<serde_json::Value>(raw).is_err(),
        "the raw reply is not valid JSON"
    );
    // The detail names the stray character, so a fault count is readable without the transcript.
    let detail = malformed_envelope_detail(raw);
    assert!(detail.contains('}'), "{detail}");
    assert!(detail.contains("NOT valid JSON"), "{detail}");
}

/// (b) A clean reply must raise NO fault. This is the guard that stops the count becoming
/// noise people learn to ignore.
#[test]
fn a_clean_reply_raises_no_malformed_envelope_fault() {
    let reg = default_registry();
    for raw in [
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"finish"}"#,
        r#"{"tool":"write_file","path":"a.py","content":"x = 1\ny = 2\n"}"#,
    ] {
        assert!(
            ParseRepair.extract(raw, &reg).is_ok(),
            "precondition: {raw} parses"
        );
        assert!(
            !has_malformed_json_envelope(raw),
            "clean reply wrongly flagged: {raw}"
        );
    }
}

/// (c) The shapes the strategy already tolerates must keep working AND stay unflagged. Each of
/// these leaves something outside the object, so a naive "any leftover bytes" detector would
/// fire on all of them — which is exactly the over-firing that makes a fault worthless.
#[test]
fn tolerated_reply_shapes_are_not_flagged_as_malformed() {
    let reg = default_registry();

    // Prose either side of the call — explicitly tolerated by the extractor.
    let prose = "Sure:\n{\"tool\":\"write_file\",\"path\":\"x\",\"content\":\"a { b } c\"}\ndone";
    assert_eq!(
        ParseRepair.extract(prose, &reg).unwrap().str("content"),
        Some("a { b } c")
    );
    assert!(!has_malformed_json_envelope(prose), "prose is not a fault");

    // A legitimate batched turn: the `<tool_call|>` separator is residue, but it is markup.
    let batched = "{\"tool\":\"read_file\",\"path\":\"test_app.py\"}<tool_call|>\
                   {\"tool\":\"create_file\",\"path\":\"app.py\",\"content\":\"x = 1\"}";
    assert_eq!(
        ParseRepair.extract(batched, &reg).unwrap().name,
        "create_file"
    );
    assert!(
        !has_malformed_json_envelope(batched),
        "a batched turn is not a malformed envelope"
    );

    // Braces INSIDE a string value. The scan is string-aware, so these leave no residue —
    // the case most likely to produce a false positive if the detector were byte-naive.
    let braces = r#"{"tool":"write_file","path":"a.rs","content":"fn go() {}"}"#;
    assert_eq!(
        ParseRepair.extract(braces, &reg).unwrap().str("content"),
        Some("fn go() {}")
    );
    assert!(!has_malformed_json_envelope(braces), "code braces are safe");

    // A reply with NO call at all is a different, already-reported failure (`NoJson`) —
    // this detector must not also claim it.
    let dicts = "I'll return {'result': 25} when given {'n': 5}.";
    assert!(ParseRepair.extract(dicts, &reg).is_err());
    assert!(
        !has_malformed_json_envelope(dicts),
        "a reply with no tool call is NoJson, not a malformed envelope"
    );

    // The repair rungs that DO fire keep working, and firing a repair is not itself this
    // fault: a truncated write is malformed, but its residue is not stray punctuation.
    let truncated =
        "{\"tool\":\"write_file\",\"path\":\"styles.css\",\"content\":\"body {\\n  color: #333;";
    assert_eq!(
        ParseRepair.extract(truncated, &reg).unwrap().name,
        "write_file"
    );
    assert!(
        !has_malformed_json_envelope(truncated),
        "a truncation is reported by its own path, not this one"
    );
}

/// The trailing brace is flagged wherever it appears, including on the batched and
/// whitespace-trailed shapes the run logs actually contain.
#[test]
fn malformed_envelope_is_detected_around_whitespace_and_batches() {
    let reg = default_registry();

    // Trailing brace followed by a newline — the common wire shape.
    let ws = "{\"tool\":\"read_file\",\"path\":\"a.txt\"}}\n";
    assert!(ParseRepair.extract(ws, &reg).is_ok());
    assert!(
        has_malformed_json_envelope(ws),
        "whitespace must not hide it"
    );

    // A stray trailing comma is the same class of envelope drift.
    let comma = "{\"tool\":\"read_file\",\"path\":\"a.txt\"},";
    assert!(has_malformed_json_envelope(comma));

    // A stray `]` (the model closed an array it never opened).
    let bracket = "{\"tool\":\"read_file\",\"path\":\"a.txt\"}]";
    assert!(has_malformed_json_envelope(bracket));
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

/// **A stray quote closing a number cost three turns in a row.**
///
/// Verbatim from a live run: `{"tool":"read_file","path":"…","start":130,"limit":60"}` —
/// well-formed apart from one character, rejected three times as "no JSON tool object
/// found in your reply", which is a misleading thing to say about a reply that plainly
/// contains one.
#[test]
fn a_stray_quote_after_a_number_is_repaired() {
    let registry = sc_tools::read_only_registry();
    let raw = r#"{"tool":"read_file","path":"crates/void_claim/src/ship_render.rs","start":130,"limit":60"}"#;
    let call = ParseRepair
        .extract(raw, &registry)
        .expect("a one-character slip must not cost a turn");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.int("limit"), Some(60));
    assert_eq!(call.int("start"), Some(130));
}

/// The repair must not touch a quote that legitimately ends a string.
#[test]
fn a_quote_ending_a_real_string_is_left_alone() {
    // `"query":"fn draw_trails"` — the quote follows `s`, not a digit, so it stays.
    let registry = sc_tools::read_only_registry();
    let raw = r#"{"tool":"search_code","query":"draw_trails"}"#;
    let call = ParseRepair.extract(raw, &registry).expect("valid JSON");
    assert_eq!(call.str("query"), Some("draw_trails"));

    // A string whose CONTENT ends in a digit must survive intact.
    let raw2 = r#"{"tool":"search_code","query":"version 60"}"#;
    let call2 = ParseRepair.extract(raw2, &registry).expect("valid JSON");
    assert_eq!(
        call2.str("query"),
        Some("version 60"),
        "a digit before the closing quote of a real string is not a stray quote"
    );
}

// ---------------------------------------------------------------------------
// Stop sequences — the run-on second call (spec 02).
//
// Measured over a 12-rung Mellum2-12B run: 91 replies emitted one complete valid
// call, closed it with `</tool_call>`, then started a SECOND call in a different
// format and ran to the 3,072-token cap. ~1,729 of 2,606 seconds — 66% of the wall
// clock — generating text the extractor discarded. The parse never lost a turn; the
// run lost the time.
// ---------------------------------------------------------------------------

/// **The fix.** `NativeTools` declares the terminator AND puts it on the request,
/// alongside (not instead of) the tools constraint.
#[test]
fn native_strategy_sets_the_tool_call_stop_sequence() {
    let reg = default_registry();
    let mut req = sc_model::GenerateRequest::new(vec![]);
    NativeTools.prepare_request(&mut req, &reg);

    assert_eq!(
        req.stop,
        vec!["</tool_call>".to_string()],
        "the one measured terminator, and only that one"
    );
    assert_eq!(
        NativeTools.stop_sequences(),
        req.stop,
        "what the strategy declares is what lands on the request"
    );
    // The constraint is still there: the stop list is additive, not a replacement.
    assert!(
        matches!(req.constraint, Some(OutputConstraint::Tools(_))),
        "native tools must survive the stop wiring: {:?}",
        req.constraint
    );
}

/// **The blast radius stays at zero for the other two strategies.**
///
/// A stop sequence that fires inside a legitimate payload truncates real output,
/// which is strictly worse than the waste it saves. Only the arm with a measurement
/// behind it gets one; `ParseRepair` and `Grammar` are byte-for-byte unchanged.
#[test]
fn parse_repair_and_grammar_set_no_stop_sequences() {
    let reg = default_registry();

    let mut plain = sc_model::GenerateRequest::new(vec![]);
    ParseRepair.prepare_request(&mut plain, &reg);
    assert!(plain.stop.is_empty(), "parse-repair: {:?}", plain.stop);
    assert!(
        plain.constraint.is_none(),
        "and still no constraint: {:?}",
        plain.constraint
    );

    let mut gbnf = sc_model::GenerateRequest::new(vec![]);
    Grammar.prepare_request(&mut gbnf, &reg);
    assert!(gbnf.stop.is_empty(), "grammar: {:?}", gbnf.stop);
    assert!(
        matches!(gbnf.constraint, Some(OutputConstraint::Grammar(_))),
        "the grammar still rides: {:?}",
        gbnf.constraint
    );

    let mut scratch_req = sc_model::GenerateRequest::new(vec![]);
    Grammar::with_scratchpad(400).prepare_request(&mut scratch_req, &reg);
    assert!(
        scratch_req.stop.is_empty(),
        "scratchpad grammar: {:?}",
        scratch_req.stop
    );

    assert!(ParseRepair.stop_sequences().is_empty());
    assert!(Grammar.stop_sequences().is_empty());
}

/// **A reply cut at the stop sequence still parses.**
///
/// The server strips the matched stop text, so what comes back is exactly the
/// complete leading call — the ~19 seconds of run-on second call that followed it in
/// the real transcript never gets generated. Feeding the reply that ends right where
/// the cut lands must yield the same call as the full rambling reply did.
#[test]
fn a_reply_cut_at_the_stop_sequence_still_yields_the_call() {
    let reg = default_registry();

    // The real shape, from the transcript: one valid call, `</tool_call>`, then a
    // second call in a different format running to the cap.
    let rambling = concat!(
        r#"{"new_str":"let x = 1;","old_str":"let x = 0;","path":"tokenizer.rs","tool":"edit_file"}"#,
        "</tool_call>\n\n",
        r#"{"name": "edit_file", "arguments": {"new_str": "let y ="#,
    );
    // What the server returns once `stop` is set: everything before the marker.
    let cut = rambling.split("</tool_call>").next().unwrap();

    let from_cut = NativeTools
        .extract(cut, &reg)
        .expect("the cut reply parses");
    assert_eq!(from_cut.name, "edit_file");
    assert_eq!(from_cut.str("path"), Some("tokenizer.rs"));
    assert_eq!(from_cut.str("new_str"), Some("let x = 1;"));

    // And it is the SAME call the full reply produced — the stop changes cost, not
    // outcome.
    let from_full = NativeTools
        .extract(rambling, &reg)
        .expect("the full reply parsed too; that was never the problem");
    assert_eq!(from_full.name, from_cut.name);
    assert_eq!(from_full.str("path"), from_cut.str("path"));
    assert_eq!(from_full.str("new_str"), from_cut.str("new_str"));
}

/// **What a misfire actually looks like, and what already survives one.**
///
/// A payload legitimately containing `</tool_call>` is the residual risk of the stop
/// list. Two outcomes, and the difference is worth pinning because it decides whether
/// the `StopSequenceMisfire` fault is the only safety net:
///
/// * A `write_file` cut mid-`content` is SALVAGED by the existing truncation repair --
///   it lands the partial content, and the model appends the rest. Degraded, not lost.
/// * An `edit_file` cut mid-`old_str` has no such recovery: an anchor that never closed
///   cannot be matched, so the turn yields nothing and looks exactly like a model that
///   declined to act. That is the case the fault exists to name.
#[test]
fn a_cut_inside_an_argument_degrades_or_yields_nothing_but_never_corrupts() {
    let reg = default_registry();

    // A write_file whose content is itself a chat template: cut mid-string, the JSON
    // never closes -- and the truncation salvage lands what did arrive.
    let write_cut = r#"{"tool":"write_file","path":"fixture.txt","content":"<tool_call>{}"#;
    let salvaged = NativeTools
        .extract(write_cut, &reg)
        .expect("the existing truncation salvage covers this shape");
    assert_eq!(salvaged.name, "write_file");
    assert_eq!(
        salvaged.str("content"),
        Some("<tool_call>{}"),
        "the partial content lands verbatim -- never spliced with the raw JSON tail"
    );

    // An edit_file cut mid-anchor has nothing to salvage: no closing quote, no
    // new_str, nothing to match against the file.
    let edit_cut = r#"{"tool":"edit_file","path":"chat.rs","old_str":"let t = "#;
    assert!(
        NativeTools.extract(edit_cut, &reg).is_err(),
        "a cut anchor must not be applied to a file"
    );
}

// ---------------------------------------------------------------------------
// The mislabelled tool call: right arguments, wrong name.
// ---------------------------------------------------------------------------

/// THE LIVE CASE. The harness told the model *"STOP editing by anchor. Instead call
/// `write_file` with `path` `pathfind.rs` and the ENTIRE corrected file contents in one shot."*
/// The model obeyed the NAME and kept its own intent — an anchored edit under a `write_file`
/// label. The turn was thrown away with `tool "write_file" has no parameter "new_str"`, which
/// teaches it nothing: it did what the harness asked. Believe the arguments.
#[test]
fn an_edit_labelled_write_file_runs_as_the_edit_it_is() {
    let reg = default_registry();
    let raw = r#"{"new_str":"/// 8-connected","old_str":"/// 4-connected","path":"pathfind.rs","tool":"write_file"}"#;
    let call = ParseRepair
        .extract(raw, &reg)
        .expect("an unambiguous edit_file key set must be recovered");
    assert_eq!(
        call.name, "edit_file",
        "the RECOVERED name must ride on the call — the journal snapshot, key_arg and the \
         stall-detector action hash all key off it"
    );
    assert_eq!(call.str("path"), Some("pathfind.rs"));
    assert_eq!(call.str("old_str"), Some("/// 4-connected"));
    assert_eq!(call.str("new_str"), Some("/// 8-connected"));
    assert!(
        !call.args.contains_key("tool"),
        "validation strips the envelope field"
    );
}

/// The note the loop appends, so the model learns the NAME rather than having its mistake
/// silently papered over.
#[test]
fn a_recovery_reports_both_the_named_and_the_recovered_tool() {
    let reg = default_registry();
    let raw = r#"{"tool":"write_file","path":"p.rs","old_str":"a","new_str":"b"}"#;
    let (named, recovered) =
        mislabelled_tool_recovery(raw, &reg).expect("the rung fired, so the loop must see it");
    assert_eq!(named, "write_file");
    assert_eq!(recovered, "edit_file");
    let note = mislabelled_tool_note(&named, &recovered);
    assert_eq!(
        note,
        "[you named write_file; your arguments were an edit_file call, so it ran as one]"
    );
}

/// AMBIGUITY IS REFUSED. `{path, content}` fits `write_file`, `create_file` AND `append_file`.
/// "The model obviously meant a write" is not good enough: appending when it meant to
/// overwrite silently corrupts the file. Today's error stands instead.
#[test]
fn an_ambiguous_key_set_is_never_recovered() {
    let reg = default_registry();
    let raw = r#"{"tool":"edit_file","path":"p.rs","content":"x"}"#;
    let err = ParseRepair
        .extract(raw, &reg)
        .expect_err("path+content matches three tools — it must not be guessed");
    assert!(
        matches!(err, RepairError::Invalid(_)),
        "falls through to the normal validation error, got {err:?}"
    );
    assert!(
        mislabelled_tool_recovery(raw, &reg).is_none(),
        "and the loop must not be told a recovery happened"
    );
    // The existing edit_file+content hint is what the model sees — unchanged.
    assert!(err.to_string().contains("write_file"));
}

/// A genuinely malformed call still produces today's error, word for word. This is the
/// fall-through the whole rung is gated behind.
#[test]
fn a_genuinely_malformed_call_keeps_todays_error() {
    let reg = default_registry();
    // `bogus` is declared by NO tool, so no key set can identify one.
    let raw = r#"{"tool":"read_file","path":"a.txt","bogus":1}"#;
    let err = ParseRepair.extract(raw, &reg).unwrap_err();
    assert_eq!(
        err,
        RepairError::Invalid(sc_tools::ValidationError::UnknownParam {
            tool: "read_file".into(),
            param: "bogus".into(),
        })
    );
    assert!(err.to_string().contains("has no parameter \"bogus\""));

    // A MISSING required param is not this rung's business either: nothing about it says
    // "wrong name", and an empty key set fits both run_verification and finish.
    let missing = ParseRepair
        .extract(r#"{"tool":"read_file"}"#, &reg)
        .unwrap_err();
    assert_eq!(
        missing,
        RepairError::Invalid(sc_tools::ValidationError::MissingParam {
            tool: "read_file".into(),
            param: "path",
        })
    );
    assert!(mislabelled_tool_recovery(r#"{"tool":"read_file"}"#, &reg).is_none());
}

/// A CORRECT call never goes near the new rung. Pinned because the recovery must be
/// unreachable from the happy path — the strict parse returns first.
#[test]
fn a_correct_call_is_untouched_by_the_rename_rung() {
    let reg = default_registry();
    for raw in [
        r#"{"tool":"edit_file","path":"p.rs","old_str":"a","new_str":"b"}"#,
        r#"{"tool":"write_file","path":"p.rs","content":"x"}"#,
        r#"{"tool":"read_file","path":"p.rs"}"#,
        r#"{"tool":"finish"}"#,
    ] {
        let call = ParseRepair.extract(raw, &reg).unwrap();
        let named = serde_json::from_str::<serde_json::Value>(raw).unwrap()["tool"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(call.name, named, "a valid call keeps its own name: {raw}");
        assert!(
            mislabelled_tool_recovery(raw, &reg).is_none(),
            "the rung must not even report on a valid call: {raw}"
        );
    }
}

/// The other shapes that ARE unique, so the rule is a rule and not one hard-coded pair —
/// and the ones that stay refused because they are not.
#[test]
fn uniqueness_is_the_rule_not_a_hard_coded_pair() {
    let reg = default_registry();

    // path+start+end+new_text is edit_lines and nothing else.
    let lines = ParseRepair
        .extract(
            r#"{"tool":"edit_file","path":"p.rs","start":3,"end":5,"new_text":"z"}"#,
            &reg,
        )
        .unwrap();
    assert_eq!(lines.name, "edit_lines");

    // path+name+new_body is edit_function and nothing else.
    let func = ParseRepair
        .extract(
            r#"{"tool":"write_file","path":"p.rs","name":"go","new_body":"fn go() {}"}"#,
            &reg,
        )
        .unwrap();
    assert_eq!(func.name, "edit_function");

    // A bare `path` fits read_file, list_dir AND profile_hotspots — never recovered. (Here
    // gate 1 already declines: edit_file fails on a MISSING param, not an unexpected one.)
    assert!(mislabelled_tool_recovery(r#"{"tool":"edit_file","path":"p.rs"}"#, &reg).is_none());

    // A key set that is unique but whose VALUES are wrong still fails: gate 3 re-validates.
    assert!(ParseRepair
        .extract(
            r#"{"tool":"write_file","path":"p.rs","old_str":"","new_str":"b"}"#,
            &reg
        )
        .is_err());
    assert!(ParseRepair
        .extract(
            r#"{"tool":"write_file","path":"p.rs","old_str":7,"new_str":"b"}"#,
            &reg
        )
        .is_err());
}

/// The rung reads the REGISTRY IT WAS GIVEN, never a hard-coded table — a trimmed menu has
/// different tools and therefore different unique key sets.
#[test]
fn the_rung_reads_the_registry_it_was_given() {
    let worker = sc_tools::minimal_worker_registry();
    // The worker menu is edit_file + edit_lines + run_verification + finish, no write_file.
    // path+old_str+new_str is still uniquely edit_file there.
    let call = ParseRepair
        .extract(
            r#"{"tool":"edit_lines","path":"p.rs","old_str":"a","new_str":"b"}"#,
            &worker,
        )
        .unwrap();
    assert_eq!(call.name, "edit_file");

    // On a registry with no edit_file at all, the same payload has nothing to recover to.
    let no_edit = worker.only(&["run_verification", "finish"]).unwrap();
    assert!(ParseRepair
        .extract(
            r#"{"tool":"finish","path":"p.rs","old_str":"a","new_str":"b"}"#,
            &no_edit
        )
        .is_err());
}

// ---------------------------------------------------------------------------
// Bounding a recovered string body: the closer is a STRUCTURAL quote, not the
// last quote anywhere. See `text::structural_closing_quote`.
// ---------------------------------------------------------------------------

#[test]
fn content_first_key_order_does_not_splice_the_envelope_into_the_file() {
    // THE LIVE BUG (rust-multi-site rung). Mellum emits `content` FIRST — its key order is
    // ['content','path','tool'] in every repaired reply — and the old bound was "the last `\"`
    // in the remaining text", which is only correct when `content` is LAST. So the harness
    // walked past the body's real closer and wrote the rest of its own JSON envelope into
    // render.rs as source, then reported compile errors the model had not caused. The model's
    // fix was FULLY correct; the harness turned a solve into an 8-turn failure.
    let raw = r#"{"content":"fn go() {}\n","path":"render.rs","tool":"write_file"}"#;
    let value = repair_file_content_call(raw).expect("content-first must still be recovered");
    assert_eq!(
        value["content"].as_str(),
        Some("fn go() {}\n"),
        "the body must stop at its own closing quote, not swallow `\",\"path\":…`"
    );
    assert_eq!(value["path"].as_str(), Some("render.rs"));
    assert_eq!(value["tool"].as_str(), Some("write_file"));
}

#[test]
fn content_last_key_order_is_unchanged() {
    // REGRESSION PIN. `content` last is the order the original code was written for and the
    // order most models emit; the new bound must produce byte-identical results there.
    let raw = "{\"tool\":\"write_file\",\"path\":\"app.py\",\"content\":\"def f():\n    \"\"\"doc\"\"\"\n    return 1\n\"}";
    let value = repair_file_content_call(raw).expect("content-last must keep working");
    assert_eq!(
        value["content"].as_str(),
        Some("def f():\n    \"\"\"doc\"\"\"\n    return 1\n")
    );
    assert_eq!(value["path"].as_str(), Some("app.py"));
}

#[test]
fn an_escaped_quote_in_the_body_is_not_the_closer_in_either_key_order() {
    // The body may legitimately contain `\"` (the downstream `unescape_json_string_lenient`
    // exists for exactly that). An escaped quote must never be mistaken for the value's end —
    // and, in the content-first order, must not stop the scan before the real closer either.
    let last = r#"{"tool":"write_file","path":"a.rs","content":"let s = \"hi\";\n"}"#;
    assert_eq!(
        repair_file_content_call(last).unwrap()["content"].as_str(),
        Some("let s = \"hi\";\n"),
        "content last"
    );
    let first = r#"{"content":"let s = \"hi\";\n","path":"a.rs","tool":"write_file"}"#;
    assert_eq!(
        repair_file_content_call(first).unwrap()["content"].as_str(),
        Some("let s = \"hi\";\n"),
        "content first"
    );
}

#[test]
fn a_body_containing_the_envelope_text_verbatim_does_not_truncate_early() {
    // The source code itself contains the literal `","path":"` — this harness's own repair
    // code does. A quote followed by more content is not at a structural boundary, so the scan
    // keeps going and the whole body survives.
    let raw = r#"{"content":"const M: &str = \"\",\"path\":\"\";\nfn go() {}\n","path":"repair.rs","tool":"write_file"}"#;
    let value = repair_file_content_call(raw).expect("recovered");
    let content = value["content"].as_str().unwrap();
    assert!(
        content.contains("fn go() {}"),
        "the body was cut at the embedded envelope text: {content:?}"
    );
    assert_eq!(value["path"].as_str(), Some("repair.rs"));
}

#[test]
fn no_structural_closer_means_none_not_a_guess() {
    // A repair path for already-malformed JSON must not guess: a wrong bound writes garbage
    // into a source file, which is the very bug this fixes. With no quote at a structural
    // boundary, return None and let the caller's existing error path run unchanged.
    let raw = "{\"tool\":\"write_file\",\"path\":\"a.rs\",\"content\":\"fn go() {} still going";
    assert!(
        repair_file_content_call(raw).is_none(),
        "no valid closer → no recovery"
    );
    // The caller's behaviour is unchanged: this shape is a TRUNCATION, and the truncation
    // salvage below it in the ladder still owns it.
    let reg = default_registry();
    let call = ParseRepair.extract(raw, &reg).expect("truncation salvage");
    assert_eq!(call.name, "write_file");
    assert!(call.str("content").unwrap().contains("fn go() {}"));
}

#[test]
fn edit_file_recovery_is_also_key_order_independent() {
    // Sibling defect, same cause: `repair_edit_file_call` bounded `old_str`+`new_str` with the
    // same blind `rfind('"')`, assuming `new_str` was the object's LAST key. With `path` after
    // it, the trailing `","path":"x.rs","tool":"edit_file` was swallowed into `new_str` and
    // would have been written into the file.
    let reg = default_registry();
    let raw = "{\"old_str\":\"let x = 1;\",\"new_str\":\"let x = 2;\",\"path\":\"m.rs\",\"tool\":\"edit_file\"}";
    // Strict parsing handles this one, so drive the repair directly to prove the bound.
    let value = repair_edit_file_call(raw).expect("recovered");
    assert_eq!(value["new_str"].as_str(), Some("let x = 2;"));
    assert_eq!(value["old_str"].as_str(), Some("let x = 1;"));
    assert_eq!(value["path"].as_str(), Some("m.rs"));
    // And the end-to-end path still agrees.
    let call = ParseRepair.extract(raw, &reg).unwrap();
    assert_eq!(call.str("new_str"), Some("let x = 2;"));
}

#[test]
fn structural_closing_quote_picks_the_boundary_quote() {
    use crate::text::structural_closing_quote;
    // The quote at 1 is inside the body (followed by `b`); the one at 3 is followed by `,` AND
    // by a well-formed tail, so it is the closer — even though a later quote also sits before
    // a `}`. This is the content-first shape.
    assert_eq!(structural_closing_quote("a\"b\",\"path\":\"x\"}"), Some(3));
    // Trailing `"}` is structural: the tail is just the brace.
    assert_eq!(structural_closing_quote("body\"}"), Some(4));
    // Whitespace between the quote and the `}` is allowed.
    assert_eq!(structural_closing_quote("body\"  }"), Some(4));
    // An escaped quote is never the closer; `\\"` (an EVEN run of backslashes) is.
    assert_eq!(structural_closing_quote("a\\\"b"), None);
    assert_eq!(structural_closing_quote("a\\\\\"}"), Some(3));
    // A `",` inside the body whose tail is NOT a valid object tail is skipped, and the real
    // closer further right wins.
    assert_eq!(
        structural_closing_quote("let a = \", not a key;\"}"),
        Some(21)
    );
    // Nothing structural at all.
    assert_eq!(structural_closing_quote("no closer here"), None);
}
