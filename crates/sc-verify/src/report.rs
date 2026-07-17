//! Structured test results (spec 04 — `run_verification` returns per-test
//! pass/fail, not a 5k-line log; spec 11 — the test is the oracle).
//!
//! A small window must be spent on *what's broken*, not a wall of green
//! ([05](../../docs/specs/05-context-management.md)): the report carries the
//! failing cases with their messages, and renders an observation that leads with
//! failures and truncates the passing noise.

/// The outcome of a single test case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub name: String,
    pub passed: bool,
    /// Failure message / assertion detail, when failed.
    pub message: Option<String>,
}

/// The structured result of running a verification command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReport {
    /// Per-test outcomes, when the framework's output could be parsed.
    pub cases: Vec<TestCase>,
    /// Whether the command exited 0 (the whole-suite gate, spec 11).
    pub command_ok: bool,
    /// True when no per-test breakdown could be parsed and we fell back to the
    /// raw exit code (the generic path).
    pub generic: bool,
    /// The command's combined stdout/stderr, kept for the generic path so a
    /// failing non-test command (e.g. `cargo check`) can show the model its
    /// actual errors instead of a bare "exited non-zero". `None` for parsed
    /// per-test reports (their detail lives on the cases).
    pub raw: Option<String>,
}

/// Max chars of raw generic output to surface in an observation — enough for a
/// compiler-error block, bounded so a 5k-line log can't blow the window. The TAIL
/// is kept (cargo prints the error summary last).
const RAW_TAIL_CHARS: usize = 2000;

impl TestReport {
    /// A generic pass/fail with no per-test detail (exit-code fallback), carrying
    /// the raw command output so a failing command can still show what broke.
    pub fn generic(command_ok: bool) -> Self {
        Self {
            cases: Vec::new(),
            command_ok,
            generic: true,
            raw: None,
        }
    }

    /// A generic report that carries the command's raw output (surfaced on failure).
    pub fn generic_with_output(command_ok: bool, output: &str) -> Self {
        Self {
            cases: Vec::new(),
            command_ok,
            generic: true,
            raw: Some(output.to_string()),
        }
    }

    pub fn passed_count(&self) -> usize {
        self.cases.iter().filter(|c| c.passed).count()
    }

    pub fn failed(&self) -> Vec<&TestCase> {
        self.cases.iter().filter(|c| !c.passed).collect()
    }

    /// The whole-suite gate (spec 11): every parsed test green *and* the command
    /// exited 0. For a generic report, just the exit code.
    pub fn all_green(&self) -> bool {
        self.command_ok && self.failed().is_empty()
    }

    /// A compact, failure-first observation for the model. Leads with failing
    /// cases and their messages; summarizes the passing ones rather than listing
    /// them (spec 05 — spend the window on what's broken).
    pub fn observation(&self) -> String {
        if self.generic {
            if self.command_ok {
                return "run_verification: command exited 0 (passed)".into();
            }
            // Failed: show the real output so the model can fix it. Lead with the actual ERROR
            // blocks (a build's warnings and its final "could not compile" summary would
            // otherwise dominate the tail, so the model fixes the wrong thing — observed live: it
            // chased a dead-code warning while the real "unexpected closing brace" error scrolled
            // past). Fall back to the tail when no error lines are recognized.
            let mut out =
                String::from("run_verification: command exited non-zero (failed).");
            if let Some(raw) = &self.raw {
                let raw = raw.trim();
                if !raw.is_empty() {
                    let body = error_first(raw);
                    out.push_str("\nOutput:\n");
                    out.push_str(&body);
                }
            }
            return out;
        }
        let failed = self.failed();
        let passed = self.passed_count();
        if failed.is_empty() {
            return format!("run_verification: all {passed} test(s) passed ✓");
        }
        let mut out = format!(
            "run_verification: {} failed, {passed} passed:\n",
            failed.len()
        );
        for c in &failed {
            out.push_str(&format!("✗ {}", c.name));
            if let Some(m) = &c.message {
                out.push_str(&format!("\n    {}", m.replace('\n', "\n    ")));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

/// From raw build/command output, produce a bounded, ERROR-FIRST slice for the model: the
/// `error[...]`/`error:` lines and the context that follows each (the `-->` location, the code
/// frame) — the stuff needed to fix the failure — with warnings dropped. If no error lines are
/// found (a runtime failure with no compiler errors), fall back to the tail of the output.
fn error_first(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        let is_error = t.starts_with("error[") || t.starts_with("error:");
        if is_error {
            // Take this error line plus its following context: indented/`-->`/frame lines, up to
            // a blank line or the next top-level diagnostic (error/warning/note at column 0).
            let mut block = vec![lines[i]];
            let mut j = i + 1;
            while j < lines.len() {
                let l = lines[j];
                let lt = l.trim_start();
                if l.trim().is_empty()
                    || lt.starts_with("error")
                    || lt.starts_with("warning")
                    || (lt.starts_with("note:") && block.len() > 1)
                {
                    break;
                }
                block.push(l);
                j += 1;
            }
            blocks.push(block.join("\n"));
            i = j;
        } else {
            i += 1;
        }
    }

    if blocks.is_empty() {
        // No compiler-error lines — fall back to the TAIL (a runtime failure/panic prints its
        // message last). Already bounded to RAW_TAIL_CHARS; keep the tail, don't re-clip the head.
        let start = raw.len().saturating_sub(RAW_TAIL_CHARS);
        let start = (start..raw.len())
            .find(|k| raw.is_char_boundary(*k))
            .unwrap_or(raw.len());
        return if start > 0 {
            format!("…\n{}", &raw[start..])
        } else {
            raw.to_string()
        };
    }

    // Lead with the error blocks (first errors = the ones to fix; later ones are often cascades),
    // bounded by keeping the HEAD.
    let joined = blocks.join("\n\n");
    if joined.chars().count() > RAW_TAIL_CHARS {
        let end = joined
            .char_indices()
            .nth(RAW_TAIL_CHARS)
            .map(|(k, _)| k)
            .unwrap_or(joined.len());
        format!("{}\n… (more errors omitted)", &joined[..end])
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, passed: bool) -> TestCase {
        TestCase {
            name: name.into(),
            passed,
            message: if passed {
                None
            } else {
                Some("assertion failed".into())
            },
        }
    }

    #[test]
    fn all_green_requires_command_ok_and_no_failures() {
        let green = TestReport {
            cases: vec![case("a", true), case("b", true)],
            command_ok: true,
            generic: false,
            raw: None,
        };
        assert!(green.all_green());

        let red = TestReport {
            cases: vec![case("a", true), case("b", false)],
            command_ok: false,
            generic: false,
            raw: None,
        };
        assert!(!red.all_green());
        assert_eq!(red.failed().len(), 1);
    }

    #[test]
    fn observation_leads_with_failures() {
        let red = TestReport {
            cases: vec![case("keep", true), case("broken", false)],
            command_ok: false,
            generic: false,
            raw: None,
        };
        let o = red.observation();
        assert!(o.contains("✗ broken"), "{o}");
        assert!(o.contains("1 failed, 1 passed"), "{o}");
        // The passing test isn't individually listed.
        assert!(!o.contains("✗ keep"), "{o}");
    }

    #[test]
    fn generic_report_uses_exit_code_only() {
        assert!(TestReport::generic(true).all_green());
        assert!(!TestReport::generic(false).all_green());
        assert!(TestReport::generic(true).observation().contains("exited 0"));
    }

    #[test]
    fn generic_failure_surfaces_the_raw_output_so_the_model_can_fix_it() {
        // The blind-flying bug: a failing `cargo check` used to say only "exited non-zero".
        // Now the observation carries the real compiler errors.
        let errs = "error[E0433]: cannot find value `lakes` in this scope\n  --> src/gen/terrain.rs:42";
        let r = TestReport::generic_with_output(false, errs);
        let o = r.observation();
        assert!(o.contains("E0433"), "compiler error surfaced: {o}");
        assert!(o.contains("terrain.rs:42"), "location surfaced: {o}");
        assert!(!r.all_green());
    }

    #[test]
    fn generic_failure_tail_is_bounded() {
        // A 5k-line log with NO compiler errors falls back to the tail, bounded.
        let big = "x\n".repeat(5000) + "panic: the real problem is here";
        let o = TestReport::generic_with_output(false, &big).observation();
        assert!(o.len() < 2400, "observation bounded, got {}", o.len());
        assert!(o.contains("the real problem is here"), "tail kept: {o}");
    }

    #[test]
    fn generic_failure_leads_with_the_error_not_warnings() {
        // The live bug: the model chased a dead-code WARNING while the real error scrolled past.
        // The observation must surface the error[...] block, not the warnings or the summary.
        let cargo = "\
warning: unused variable: `x`\n  --> src/a.rs:1\n\n\
error[E0765]: unterminated double quote string\n  --> src/gen/terrain.rs:265:9\n   |\n\
265 |         let c = \"oops;\n   |                 ^^^^^^^\n\n\
warning: fields `width` and `height` are never read\n  --> src/b.rs:2\n\n\
error: could not compile `city` (bin \"city\") due to 1 previous error\n";
        let o = TestReport::generic_with_output(false, cargo).observation();
        assert!(o.contains("E0765"), "the real error is surfaced: {o}");
        assert!(o.contains("unterminated double quote"), "{o}");
        assert!(o.contains("terrain.rs:265"), "with its location: {o}");
        // The dead-code warning is dropped (not what the model should fix).
        assert!(!o.contains("never read"), "warnings dropped: {o}");
    }
}
