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

    // Verbose per-test lines.
    for line in output.lines() {
        let line = line.trim();
        for status in ["PASSED", "FAILED", "ERROR"] {
            if let Some(name) = line.strip_suffix(status).map(str::trim) {
                if !name.is_empty() && (name.contains("::") || name.contains(".py")) {
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
                    if name.contains("::") || name.contains(".py") {
                        cases.push(TestCase {
                            name: name.to_string(),
                            passed: false,
                            message: msg,
                        });
                    }
                }
            }
        }
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
