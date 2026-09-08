//! Simplifier tests.
//!
//! Two properties matter, and only one of them is "it got smaller":
//!
//! * **It keeps what the caller needs.** A simplifier that halves the bytes and
//!   drops the failing assertion has made the run worse, not cheaper. Every
//!   reduction test below asserts on what SURVIVED, not just on the size.
//! * **It never drops silently.** Anything removed is named in the trace, so a
//!   capability that found nothing and a simplifier that ate the answer are
//!   distinguishable after the fact.

use sc_gateway::{simplify, Level, Trace};

fn extract(raw: &str) -> (String, Trace) {
    let mut trace = Trace::default();
    let out = simplify(raw, Level::Extract, &mut trace, None);
    (out, trace)
}

// ---------------------------------------------------------------------------
// Test output: the failures are the signal, the passes are not.
// ---------------------------------------------------------------------------

const TEST_OUTPUT: &str = "\
running 6 tests
test parses_empty ... ok
test parses_nested ... ok
test rejects_bad_utf8 ... ok
test handles_timeout ... FAILED
test parses_unicode ... ok
test retries_once ... ok

failures:

---- handles_timeout stdout ----
thread 'handles_timeout' panicked at crates/sc-core/src/agent/stall.rs:88:9:
assertion `left == right` failed
  left: 3
 right: 5

failures:
    handles_timeout

test result: FAILED. 5 passed; 1 failed; 0 ignored";

#[test]
fn test_output_keeps_the_failure_and_drops_the_passes() {
    let (out, trace) = extract(TEST_OUTPUT);

    // What must survive: the failing test, the file:line, and the assertion.
    assert!(
        out.contains("handles_timeout"),
        "lost the failing test name"
    );
    assert!(
        out.contains("crates/sc-core/src/agent/stall.rs:88:9"),
        "lost the failure location"
    );
    assert!(out.contains("left: 3"), "lost the assertion detail");
    assert!(out.contains("test result: FAILED"), "lost the summary line");

    // What must go: the passing lines.
    assert!(!out.contains("parses_empty ... ok"), "kept a passing test");
    assert!(!out.contains("retries_once"), "kept a passing test");

    // And the removal is on the record.
    assert!(
        trace.dropped.iter().any(|d| d.contains("passing test")),
        "dropped passes without recording it: {:?}",
        trace.dropped
    );
    assert!(out.len() < TEST_OUTPUT.len());
}

#[test]
fn an_all_green_run_still_reports_the_result() {
    let green = "\
running 2 tests
test a ... ok
test b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored";
    let (out, _) = extract(green);
    assert!(
        out.contains("test result: ok"),
        "an all-green run must still say so: {out}"
    );
}

// ---------------------------------------------------------------------------
// Compiler output: errors are actionable, warnings are not — but their absence
// must never read as a clean build.
// ---------------------------------------------------------------------------

const COMPILER_OUTPUT: &str = "\
warning: unused variable: `x`
 --> src/lib.rs:4:9
  |
4 |     let x = 1;
  |         ^ help: if this is intentional, prefix it with an underscore

warning: unused import: `std::fmt`
 --> src/lib.rs:1:5

error[E0308]: mismatched types
 --> src/lib.rs:12:5
  |
12|     returns_string()
  |     ^^^^^^^^^^^^^^^^ expected `usize`, found `String`

error: aborting due to 1 previous error";

#[test]
fn compiler_output_keeps_errors_and_counts_the_warnings() {
    let (out, trace) = extract(COMPILER_OUTPUT);

    assert!(out.contains("error[E0308]"), "lost the error code");
    assert!(out.contains("src/lib.rs:12:5"), "lost the error location");
    assert!(out.contains("expected `usize`"), "lost the error detail");

    assert!(!out.contains("unused variable"), "kept a warning");

    // The count is the point: silence about warnings would read as a clean build.
    assert!(
        trace.dropped.iter().any(|d| d.contains("2 warnings")),
        "warning count not reported: {:?}",
        trace.dropped
    );
}

#[test]
fn output_with_no_warnings_is_left_alone() {
    let only_errors = "error[E0425]: cannot find value `foo` in this scope\n --> src/a.rs:9:5";
    let (out, trace) = extract(only_errors);
    assert!(out.contains("E0425"));
    assert!(
        trace.dropped.iter().all(|d| !d.contains("warning")),
        "reported dropping warnings when there were none"
    );
}

// ---------------------------------------------------------------------------
// The general case: an unrecognised shape must survive intact. Guessing at an
// unknown format is exactly how a simplifier eats the one line that mattered.
// ---------------------------------------------------------------------------

#[test]
fn unknown_shapes_are_not_guessed_at() {
    let arbitrary = "alpha\nbeta\ngamma\ndelta";
    let (out, trace) = extract(arbitrary);
    assert_eq!(out, arbitrary, "mangled an unrecognised output shape");
    assert!(trace.dropped.is_empty());
}

#[test]
fn blank_line_runs_collapse_and_say_so() {
    let padded = "first\n\n\n\n\nsecond";
    let (out, trace) = extract(padded);
    assert_eq!(out, "first\n\nsecond");
    assert!(trace.dropped.iter().any(|d| d.contains("blank lines")));
}

#[test]
fn empty_input_is_handled() {
    let (out, trace) = extract("");
    assert!(out.is_empty());
    assert_eq!(trace.retained_percent(), 100);
}

// ---------------------------------------------------------------------------
// The raw ceiling, and the boundary bug it would otherwise hide.
// ---------------------------------------------------------------------------

#[test]
fn oversized_output_is_capped_and_the_cut_is_recorded() {
    let huge = "x".repeat(sc_gateway::MAX_RAW_CHARS + 5_000);
    let (out, trace) = extract(&huge);
    assert!(out.len() <= sc_gateway::MAX_RAW_CHARS);
    assert!(
        trace.dropped.iter().any(|d| d.contains("raw ceiling")),
        "truncated without recording it"
    );
    assert_eq!(trace.raw_bytes, huge.len());
}

#[test]
fn truncation_never_splits_a_multibyte_character() {
    // A cut mid-character panics on the next str operation. The ceiling lands
    // inside a 3-byte char here by construction.
    let unit = "日本語";
    let huge = unit.repeat(sc_gateway::MAX_RAW_CHARS);
    let (out, _) = extract(&huge);
    assert!(out.is_char_boundary(out.len()));
    assert!(out.len() <= sc_gateway::MAX_RAW_CHARS);
}

// ---------------------------------------------------------------------------
// Summarization: the lossy path. It must be opt-in, marked, and safe when the
// model is absent or fails.
// ---------------------------------------------------------------------------

#[test]
fn summarize_without_a_backend_falls_back_to_extraction() {
    let mut trace = Trace::default();
    let out = simplify(
        TEST_OUTPUT,
        Level::Summarize { max_chars: 10 },
        &mut trace,
        None,
    );
    assert!(!trace.summarized, "claimed to summarize with no backend");
    assert!(out.contains("handles_timeout"), "lost the failure");
}

#[test]
fn summarize_skips_the_model_when_output_already_fits() {
    // Paying for a model call to shorten something already under budget is pure
    // loss — and it makes the output lossy for no benefit.
    let backend = sc_model::MockBackend::new(["SHOULD NOT BE CALLED"]);
    let mut trace = Trace::default();
    let out = simplify(
        "short output",
        Level::Summarize { max_chars: 4096 },
        &mut trace,
        Some(&backend),
    );
    assert!(!trace.summarized, "summarized something already small");
    assert_eq!(out, "short output");
}

#[test]
fn summarize_marks_the_output_as_lossy() {
    let backend = sc_model::MockBackend::new(["stall.rs:88 handles_timeout: left 3 right 5"]);
    let mut trace = Trace::default();
    let out = simplify(
        TEST_OUTPUT,
        Level::Summarize { max_chars: 50 },
        &mut trace,
        Some(&backend),
    );
    assert!(trace.summarized, "lossy summary not flagged as such");
    assert!(
        trace.dropped.iter().any(|d| d.contains("summarized")),
        "summary not recorded in dropped: {:?}",
        trace.dropped
    );
    assert!(out.contains("handles_timeout"));
}

#[test]
fn a_failed_summarizer_returns_the_extracted_text_not_nothing() {
    // MockBackend errors once its script is exhausted. Too long beats absent:
    // losing the output entirely because a compression step failed would turn a
    // cost optimisation into a correctness bug.
    let backend = sc_model::MockBackend::new(Vec::<String>::new());
    let mut trace = Trace::default();
    let out = simplify(
        TEST_OUTPUT,
        Level::Summarize { max_chars: 10 },
        &mut trace,
        Some(&backend),
    );
    assert!(!trace.summarized);
    assert!(
        out.contains("handles_timeout"),
        "lost the output on failure"
    );
    assert!(
        trace.dropped.iter().any(|d| d.contains("failed")),
        "silent summarizer failure: {:?}",
        trace.dropped
    );
}

// ---------------------------------------------------------------------------
// The measurement itself.
// ---------------------------------------------------------------------------

#[test]
fn retained_percent_reports_the_reduction() {
    let (_, trace) = extract(TEST_OUTPUT);
    let pct = trace.retained_percent();
    assert!(pct < 100, "reported no reduction on reducible output");
    assert!(pct > 0, "reported total loss");
    assert_eq!(trace.raw_bytes, TEST_OUTPUT.len());
}
