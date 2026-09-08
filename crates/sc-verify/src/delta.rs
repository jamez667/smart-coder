//! What changed since the last run of the same verification command.
//!
//! A small model reads each `run_verification` result on its own and cannot tell
//! whether its last edit helped: "2 failed" after "2 failed" looks like no news even
//! when it fixed one test and broke another. The delta says so in one clause --
//! `same 2 failures as last run`, or `newly failing: a; now passing: b` -- so the
//! model learns from the run instead of re-reading the same failure list.
//!
//! The previous signature lives in a thread-local keyed by the command string: the
//! agent loop runs its verifications on one thread, and two workspaces on the same
//! thread would normally run different commands. Nothing here is persisted.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::report::{compile_errors, TestReport};

thread_local! {
    /// command -> the failure signature of its most recent run on this thread.
    static LAST_RUN: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
}

/// How many names a delta clause lists before saying `+N more`.
const MAX_NAMED: usize = 3;

/// The failure signature of a report: the sorted names of what failed. Test names for
/// a parsed report; `file:line` for a generic report whose output carries compiler
/// errors; one placeholder when a generic command failed with nothing recognisable.
/// Empty means green.
fn signature(report: &TestReport) -> Vec<String> {
    let mut names: Vec<String> = if report.generic {
        if report.command_ok {
            Vec::new()
        } else {
            let errs: Vec<String> = compile_errors(report.raw.as_deref().unwrap_or(""))
                .into_iter()
                .map(|e| format!("{}:{}", e.file, e.line))
                .collect();
            if errs.is_empty() {
                vec!["command exited non-zero".to_string()]
            } else {
                errs
            }
        }
    } else {
        report.failed().iter().map(|c| c.name.clone()).collect()
    };
    names.sort_unstable();
    names.dedup();
    names
}

/// Compare `report` against the previous run of `command` on this thread, remember it
/// as the new previous run, and return the delta clause -- or `None` when this is the
/// first run, or green followed green.
pub fn note_run(command: &str, report: &TestReport) -> Option<String> {
    let now = signature(report);
    let prev = LAST_RUN.with(|l| l.borrow_mut().insert(command.to_string(), now.clone()))?;
    describe(&prev, &now)
}

/// The delta clause between two signatures.
fn describe(prev: &[String], now: &[String]) -> Option<String> {
    if prev == now {
        return match now.len() {
            0 => None,
            1 => Some("same 1 failure as last run".to_string()),
            n => Some(format!("same {n} failures as last run")),
        };
    }
    let newly: Vec<&str> = now
        .iter()
        .filter(|n| !prev.contains(n))
        .map(String::as_str)
        .collect();
    let fixed: Vec<&str> = prev
        .iter()
        .filter(|n| !now.contains(n))
        .map(String::as_str)
        .collect();
    let mut parts = Vec::new();
    if !newly.is_empty() {
        parts.push(format!("newly failing: {}", list(&newly)));
    }
    if !fixed.is_empty() {
        parts.push(format!("now passing: {}", list(&fixed)));
    }
    Some(parts.join("; "))
}

fn list(names: &[&str]) -> String {
    let shown = names
        .iter()
        .take(MAX_NAMED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > MAX_NAMED {
        format!("{shown} +{} more", names.len() - MAX_NAMED)
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::TestCase;

    fn red(names: &[&str]) -> TestReport {
        TestReport {
            cases: names
                .iter()
                .map(|n| TestCase {
                    name: n.to_string(),
                    passed: false,
                    message: None,
                })
                .collect(),
            command_ok: false,
            generic: false,
            raw: None,
            delta: None,
        }
    }

    fn s(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn the_first_run_has_nothing_to_compare_against() {
        assert_eq!(note_run("cmd-first", &red(&["a"])), None);
    }

    #[test]
    fn an_unchanged_failure_set_says_so_with_its_count() {
        assert_eq!(
            describe(&s(&["a", "b"]), &s(&["a", "b"])).as_deref(),
            Some("same 2 failures as last run")
        );
        assert_eq!(
            describe(&s(&["a"]), &s(&["a"])).as_deref(),
            Some("same 1 failure as last run")
        );
        // Green after green is not news.
        assert_eq!(describe(&[], &[]), None);
    }

    #[test]
    fn a_changed_set_names_what_broke_and_what_was_fixed() {
        assert_eq!(
            describe(&s(&["a", "b"]), &s(&["b", "c"])).as_deref(),
            Some("newly failing: c; now passing: a")
        );
        assert_eq!(describe(&s(&["a"]), &[]).as_deref(), Some("now passing: a"));
        assert_eq!(
            describe(&[], &s(&["z"])).as_deref(),
            Some("newly failing: z")
        );
    }

    #[test]
    fn long_lists_are_capped() {
        let many = s(&["a", "b", "c", "d", "e"]);
        assert_eq!(
            describe(&[], &many).as_deref(),
            Some("newly failing: a, b, c +2 more")
        );
    }

    #[test]
    fn a_generic_failure_is_keyed_on_its_compile_errors() {
        let out = "error[E0425]: cannot find value `x`\n  --> src/a.rs:4:5\n";
        let r = TestReport::generic_with_output(false, out);
        assert_eq!(signature(&r), vec!["src/a.rs:4".to_string()]);
        // Nothing recognisable: one placeholder, so a repeat still reads as "same".
        let bare = TestReport::generic_with_output(false, "boom");
        assert_eq!(
            signature(&bare),
            vec!["command exited non-zero".to_string()]
        );
        assert!(signature(&TestReport::generic(true)).is_empty());
    }

    #[test]
    fn successive_runs_of_one_command_are_compared_in_order() {
        let cmd = "cmd-seq";
        assert_eq!(note_run(cmd, &red(&["a", "b"])), None);
        assert_eq!(
            note_run(cmd, &red(&["a", "b"])).as_deref(),
            Some("same 2 failures as last run")
        );
        assert_eq!(
            note_run(cmd, &red(&["b"])).as_deref(),
            Some("now passing: a")
        );
        // A different command has its own history.
        assert_eq!(note_run("cmd-other", &red(&["b"])), None);
    }
}
