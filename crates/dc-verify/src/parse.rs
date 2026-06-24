//! Framework-specific test-output parsers (spec 04 — structured results).
//!
//! Each parser turns a command's combined stdout/stderr + exit status into a
//! [`TestReport`]. Detection is by command string and output shape; anything we
//! can't parse falls back to a generic exit-code report so `run_verification`
//! always returns *something* structured. Adding a framework is a new parser + a
//! detection rule — the loop and tools don't change.

use crate::report::{TestCase, TestReport};

/// Which framework a command's output looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Cargo,
    Pytest,
    Generic,
}

/// Guess the framework from the command line and its output.
pub fn detect(command: &str, output: &str) -> Framework {
    let c = command.to_ascii_lowercase();
    if c.contains("cargo") && c.contains("test") {
        return Framework::Cargo;
    }
    if c.contains("pytest") || c.contains("py.test") {
        return Framework::Pytest;
    }
    // Fall back to output sniffing for wrappers that hide the runner.
    if output.contains("running ") && output.contains("test result:") {
        return Framework::Cargo;
    }
    if output.contains("=== ") && (output.contains(" passed") || output.contains(" failed")) {
        return Framework::Pytest;
    }
    Framework::Generic
}

/// Parse `output`/`command_ok` for `command` into a structured report.
pub fn parse(command: &str, output: &str, command_ok: bool) -> TestReport {
    match detect(command, output) {
        Framework::Cargo => parse_cargo(output, command_ok),
        Framework::Pytest => parse_pytest(output, command_ok),
        Framework::Generic => TestReport::generic(command_ok),
    }
}

/// Parse `cargo test` libtest output, e.g.:
/// `test mod::it_works ... ok` / `test mod::it_breaks ... FAILED`, plus the
/// `---- mod::it_breaks stdout ----` failure detail blocks.
fn parse_cargo(output: &str, command_ok: bool) -> TestReport {
    let mut cases = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("test ") {
            // "name ... ok" | "name ... FAILED" | "name ... ignored"
            if let Some((name, status)) = rest.rsplit_once(" ... ") {
                let name = name.trim();
                match status.trim() {
                    "ok" => cases.push(TestCase {
                        name: name.to_string(),
                        passed: true,
                        message: None,
                    }),
                    "FAILED" => cases.push(TestCase {
                        name: name.to_string(),
                        passed: false,
                        message: None,
                    }),
                    _ => {} // ignored / measured — not a pass/fail signal
                }
            }
        }
    }
    attach_cargo_failure_messages(output, &mut cases);
    if cases.is_empty() {
        return TestReport::generic(command_ok);
    }
    TestReport {
        cases,
        command_ok,
        generic: false,
    }
}

/// Pull the `---- <name> stdout ----` panic/assertion blocks and attach them to
/// the matching failed case.
fn attach_cargo_failure_messages(output: &str, cases: &mut [TestCase]) {
    let lines: Vec<&str> = output.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let l = line.trim();
        if let Some(name) = l
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            // Gather until a blank line or the next block.
            let mut msg = Vec::new();
            for next in &lines[i + 1..] {
                let t = next.trim();
                if t.is_empty() || t.starts_with("---- ") {
                    break;
                }
                msg.push(t);
            }
            if let Some(c) = cases.iter_mut().find(|c| c.name == name && !c.passed) {
                if !msg.is_empty() {
                    c.message = Some(msg.join("\n"));
                }
            }
        }
    }
}

/// Parse pytest output, handling both verbose and quiet (`-q`) modes:
///
/// * **verbose** (`-v`): per-test lines `path::test_name PASSED|FAILED|ERROR`.
/// * **quiet** (`-q`, the common case): only the `short test summary info` lines
///   `FAILED path::test - reason` / `ERROR path::test - reason` for failures, plus
///   a final `N passed, M failed` count. We surface every failing case with its
///   reason and synthesize placeholder passes so the count is right.
fn parse_pytest(output: &str, command_ok: bool) -> TestReport {
    let mut cases = Vec::new();

    // Verbose per-test lines: `path::test_name STATUS` (the STATUS is the LAST token).
    // Guard against captured-log noise — pytest echoes Flask's logger lines like
    // `ERROR    app:app.py:875 Exception on /sum [POST]` where the level is the FIRST
    // token and the line merely *contains* `.py`. Require a real pytest node id (has
    // `::`) or a test-file name, so a stray log line is never counted as a test.
    for line in output.lines() {
        let line = line.trim();
        for status in ["PASSED", "FAILED", "ERROR"] {
            if let Some(name) = line.strip_suffix(status).map(str::trim) {
                if !name.is_empty() && is_pytest_node(name) {
                    cases.push(TestCase {
                        name: name.to_string(),
                        passed: status == "PASSED",
                        message: (status != "PASSED").then(|| status.to_string()),
                    });
                }
            }
        }
    }

    // Quiet-mode summary lines: `FAILED path::test - message` / `ERROR ... - ...`.
    if cases.is_empty() {
        for line in output.lines() {
            let line = line.trim();
            for kind in ["FAILED ", "ERROR "] {
                if let Some(rest) = line.strip_prefix(kind) {
                    let (name, msg) = match rest.split_once(" - ") {
                        Some((n, m)) => (n.trim(), Some(m.trim().to_string())),
                        None => (rest.trim(), Some(kind.trim().to_string())),
                    };
                    if is_pytest_node(name) {
                        cases.push(TestCase {
                            name: name.to_string(),
                            passed: false,
                            message: msg,
                        });
                    }
                }
            }
        }
        // A collection ERROR (e.g. the source file has a syntax error, or an import
        // fails) shows in the summary as a bare `ERROR path.py` with no ` - reason`;
        // the real traceback lives in an `___ ERROR collecting path.py ___` block
        // above. Attach the last line of that block (the actual exception) so the
        // model sees WHAT broke, not a blind "ERROR".
        attach_pytest_collection_errors(output, &mut cases);

        // A test whose only message is a bare assertion (e.g. `assert 500 == 200` from a
        // route that raised) hides the ROOT cause: the exception is in the failure-detail
        // block (`___ test_x ___` with `E   NameError: ...`). Without it the model loops
        // blindly (observed live 2026-06-15: a route 500'd on a missing `render_template`
        // import and the agent never learned why). Append the underlying exception.
        attach_pytest_failure_exceptions(output, &mut cases);

        // Add placeholder passes from the summary so passed/total is meaningful
        // (e.g. "2 failed, 1 passed in 0.05s").
        if let Some(passed) = pytest_summary_passed(output) {
            for i in 0..passed {
                cases.push(TestCase {
                    name: format!("(passed #{})", i + 1),
                    passed: true,
                    message: None,
                });
            }
        }
    }

    if cases.is_empty() {
        return TestReport::generic(command_ok);
    }
    TestReport {
        cases,
        command_ok,
        generic: false,
    }
}

/// Whether `name` looks like a real pytest node id, not a captured-log line. A node
/// is either `path::test_...` (the common case) or a bare test-file name like
/// `test_foo.py` / `foo_test.py` (collection errors). Rejects Flask logger noise such
/// as `app:app.py:875 Exception on /sum [POST]` — it has a single `:` (not `::`), an
/// embedded `:<line>`, and whitespace in the "name".
fn is_pytest_node(name: &str) -> bool {
    if name.contains("::") {
        // A node id's left side is a path; the right side a test name. Reject if it
        // carries spaces (a log message) before the first `::`.
        return !name.split("::").next().unwrap_or("").contains(' ');
    }
    // Bare file name (collection error): `<something>test<something>.py` with no spaces
    // and no `:line` suffix.
    let f = name.trim();
    !f.contains(' ')
        && !f.contains(':')
        && f.ends_with(".py")
        && (f.contains("test_") || f.contains("_test"))
}

/// For each FAILED test, look in its `____ <test> ____` failure-detail block for the
/// underlying exception (`E   NameError: ...`) and append it to the message when the
/// message alone is a bare assertion. A route that raises shows in the summary only as
/// `assert 500 == 200`; the real cause (e.g. `NameError: name 'render_template' is not
/// defined`) lives in the traceback. Surfacing it is the difference between the model
/// fixing the import and looping blindly.
fn attach_pytest_failure_exceptions(output: &str, cases: &mut [TestCase]) {
    let lines: Vec<&str> = output.lines().collect();
    for case in cases.iter_mut() {
        if case.passed {
            continue;
        }
        // The test name in the detail header is the last `::` segment.
        let short = case.name.rsplit("::").next().unwrap_or(&case.name);
        // Find the `____ <short> ____` failure block header.
        let Some(start) = lines.iter().position(|l| {
            let t = l.trim_matches('_').trim();
            (t == short || t.starts_with(&format!("{short} "))) && l.trim_start().starts_with('_')
        }) else {
            continue;
        };
        // Scan the block (including any `--- Captured log call ---` traceback that
        // follows, up to the next test/section header) for the REAL exception. Two
        // shapes: pytest's assertion-rewrite `E   <Exc>: ...` lines, AND a raw
        // traceback's final `<Exc>: ...` line (a route that raised logs its traceback
        // via Flask — the cause has NO `E ` prefix). Keep the last named-exception line.
        let mut exc: Option<String> = None;
        for next in &lines[start + 1..] {
            let t = next.trim();
            // Stop at the next failure block or the final summary, but DON'T stop at a
            // `---`-ruled sub-section (that's where Captured log / traceback lives).
            if t.starts_with("___") || t.starts_with("===") {
                break;
            }
            let candidate = t.strip_prefix("E ").map(str::trim).unwrap_or(t);
            // A named exception line looks like `SomeError: message` / `SomeException: …`.
            let is_named_exc = (candidate.contains("Error") || candidate.contains("Exception"))
                && candidate.contains(':')
                && !candidate.starts_with("File ");
            if is_named_exc {
                exc = Some(candidate.to_string());
            }
        }
        if let Some(e) = exc {
            // Append the exception unless the message already carries it.
            match &case.message {
                Some(m) if m.contains(&e) => {}
                Some(m) => case.message = Some(format!("{m}  ({e})")),
                None => case.message = Some(e),
            }
        }
    }
}

/// For each ERROR case whose message is still the bare placeholder, find the
/// matching `___ ERROR collecting <name> ___` block and attach the real exception
/// line (e.g. `ModuleNotFoundError: No module named 'run'`) so the model can act.
fn attach_pytest_collection_errors(output: &str, cases: &mut [TestCase]) {
    let lines: Vec<&str> = output.lines().collect();
    for case in cases.iter_mut() {
        // Only ERROR cases left with the placeholder message ("ERROR").
        if case.passed || case.message.as_deref() != Some("ERROR") {
            continue;
        }
        // The collection block header names the file: "ERROR collecting <file>".
        let file = case.name.split("::").next().unwrap_or(&case.name);
        if let Some(start) = lines.iter().position(|l| {
            let t = l.trim_matches('_').trim();
            t.starts_with("ERROR collecting") && t.contains(file)
        }) {
            // The exception line is the last non-empty line of the block (before the
            // next `___`-ruled header or the short-summary section).
            let mut exc: Option<String> = None;
            for next in &lines[start + 1..] {
                let t = next.trim();
                if t.is_empty() {
                    continue;
                }
                if t.starts_with("___") || t.starts_with("===") {
                    break;
                }
                // Keep the most specific line: an `Error:`/`Exception` line wins.
                if t.contains("Error") || t.contains("Exception") || exc.is_none() {
                    exc = Some(t.to_string());
                }
            }
            if let Some(e) = exc {
                case.message = Some(e);
            }
        }
    }
}

/// Pull the "N passed" count from pytest's final summary line, if present
/// (e.g. "2 failed, 1 passed in 0.05s" → 1).
fn pytest_summary_passed(output: &str) -> Option<usize> {
    for line in output.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        for (i, t) in toks.iter().enumerate() {
            if *t == "passed" && i > 0 {
                if let Ok(n) = toks[i - 1].parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_frameworks() {
        assert_eq!(detect("cargo test", ""), Framework::Cargo);
        assert_eq!(detect("python -m pytest -q", ""), Framework::Pytest);
        assert_eq!(detect("sh test.sh", ""), Framework::Generic);
        // Output sniffing when the command is a wrapper.
        assert_eq!(
            detect("make test", "running 1 test\ntest result: ok."),
            Framework::Cargo
        );
    }

    #[test]
    fn parses_cargo_pass_and_fail_with_messages() {
        let out = "\
running 2 tests
test suite::it_works ... ok
test suite::it_breaks ... FAILED

failures:

---- suite::it_breaks stdout ----
thread 'suite::it_breaks' panicked at src/lib.rs:9:5:
assertion `left == right` failed
  left: 1
  right: 2

test result: FAILED. 1 passed; 1 failed; 0 ignored;
";
        let report = parse("cargo test", out, false);
        assert!(!report.generic);
        assert_eq!(report.passed_count(), 1);
        let failed = report.failed();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "suite::it_breaks");
        assert!(
            failed[0]
                .message
                .as_ref()
                .unwrap()
                .contains("left == right"),
            "msg: {:?}",
            failed[0].message
        );
        assert!(!report.all_green());
    }

    #[test]
    fn parses_cargo_all_green() {
        let out = "\
running 1 test
test suite::ok_test ... ok

test result: ok. 1 passed; 0 failed;
";
        let report = parse("cargo test", out, true);
        assert!(report.all_green());
        assert_eq!(report.passed_count(), 1);
    }

    #[test]
    fn parses_pytest_verbose() {
        let out = "\
tests/test_core.py::test_add PASSED
tests/test_core.py::test_sub FAILED
tests/test_core.py::test_mul PASSED
";
        let report = parse("pytest -v", out, false);
        assert!(!report.generic);
        assert_eq!(report.passed_count(), 2);
        assert_eq!(report.failed().len(), 1);
        assert_eq!(report.failed()[0].name, "tests/test_core.py::test_sub");
    }

    #[test]
    fn parses_pytest_quiet_mode_summary() {
        // `pytest -q` output: dots, then short-summary FAILED lines + counts.
        let out = "\
..F                                                                      [100%]
=================================== FAILURES ===================================
=========================== short test summary info ===========================
FAILED test_calc.py::test_four_is_even - assert False is True
FAILED test_calc.py::test_ten_is_even - assert False is True
2 failed, 1 passed in 0.05s
";
        let report = parse("python -m pytest -q", out, false);
        assert!(
            !report.generic,
            "quiet mode should parse, not fall to generic"
        );
        assert_eq!(report.failed().len(), 2);
        assert_eq!(report.passed_count(), 1); // from the "1 passed" summary
        assert!(report.failed()[0].name.contains("test_four_is_even"));
        assert!(report
            .failed()
            .iter()
            .any(|c| c.message.as_deref() == Some("assert False is True")));
        assert!(!report.all_green());
    }

    #[test]
    fn pytest_collection_error_surfaces_the_exception_not_just_error() {
        // A source file that fails to import shows as a bare `ERROR <file>` in the
        // summary with the real traceback in an `ERROR collecting` block above. The
        // model must see the exception, not a blind "ERROR".
        let out = "\
==================================== ERRORS ====================================
_______________________ ERROR collecting test_run.py _______________________
test_run.py:2: in <module>
    from run import app
E   ModuleNotFoundError: No module named 'run'
=========================== short test summary info ===========================
ERROR test_run.py
1 error in 0.04s
";
        let report = parse("python -m pytest -q", out, false);
        assert!(!report.generic, "should parse: {out}");
        let failed = report.failed();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "test_run.py");
        assert_eq!(
            failed[0].message.as_deref(),
            Some("E   ModuleNotFoundError: No module named 'run'"),
            "the exception is attached, not the bare placeholder"
        );
        assert!(!report.all_green());
    }

    #[test]
    fn pytest_failure_surfaces_the_underlying_exception_not_just_the_assert() {
        // A route that raises shows in the summary as `assert 500 == 200`; the real
        // cause is in the failure block. The model must SEE the NameError to fix it.
        // The REAL format observed live: the exception is in a `Captured log call`
        // traceback (NO `E ` prefix), not in the assertion-rewrite block.
        let out = "\
=================================== FAILURES ===================================
__________________________ test_root_renders_template __________________________

    def test_root_renders_template():
        c = app.test_client()
        r = c.get('/')
>       assert r.status_code == 200
E       assert 500 == 200
E        +  where 500 = <WrapperTestResponse streamed [500 INTERNAL SERVER ERROR]>.status_code

test_app.py:6: AssertionError
------------------------------ Captured log call -------------------------------
ERROR    app:app.py:875 Exception on / [GET]
Traceback (most recent call last):
  File \"/workspace/app.py\", line 7, in index
    return app.render_template('index.html')
AttributeError: 'Flask' object has no attribute 'render_template'
=========================== short test summary info ============================
FAILED test_app.py::test_root_renders_template - assert 500 == 200
1 failed in 0.12s
";
        let report = parse("python -m pytest -q", out, false);
        let failed = report.failed();
        assert_eq!(failed.len(), 1);
        let msg = failed[0].message.as_deref().unwrap_or("");
        assert!(
            msg.contains("AttributeError") && msg.contains("render_template"),
            "the underlying exception must be surfaced, got: {msg:?}"
        );
    }

    #[test]
    fn flask_log_lines_are_not_counted_as_tests() {
        // pytest echoes Flask's captured logger output. A line like
        // `ERROR    app:app.py:875 Exception on /sum [POST]` ends in nothing useful but
        // earlier code matched any `.py`-containing line ending in a status word and
        // invented a phantom failing "test". It must be ignored; only the real node id
        // counts.
        let out = "\
test_app.py::test_health PASSED
ERROR    app:app.py:875 Exception on /sum [POST]
test_app.py::test_sum_invalid_json FAILED
";
        let report = parse("python -m pytest -v", out, false);
        assert!(!report.generic);
        let failed = report.failed();
        assert_eq!(failed.len(), 1, "only the real test failed: {failed:?}");
        assert_eq!(failed[0].name, "test_app.py::test_sum_invalid_json");
        assert_eq!(report.passed_count(), 1);
    }

    #[test]
    fn is_pytest_node_accepts_nodes_rejects_logs() {
        assert!(is_pytest_node("test_app.py::test_health"));
        assert!(is_pytest_node("tests/test_core.py::test_add"));
        assert!(is_pytest_node("test_run.py")); // collection-error file
        assert!(!is_pytest_node("app:app.py:875 Exception on /sum [POST]"));
        assert!(!is_pytest_node("app.py")); // a source file, not a test
        assert!(!is_pytest_node("in app: Exception on /sum [POST]"));
    }

    #[test]
    fn unrecognized_output_is_generic() {
        let report = parse("sh test.sh", "some custom output\n", true);
        assert!(report.generic);
        assert!(report.all_green());
    }

    #[test]
    fn cargo_with_no_test_lines_falls_back_to_generic() {
        // A cargo invocation that compiled but ran no tests still yields a report.
        let report = parse("cargo test", "Compiling foo v0.1.0\nFinished\n", true);
        assert!(report.generic);
    }
}
