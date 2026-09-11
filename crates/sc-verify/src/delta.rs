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
//!
//! THAT ASSUMPTION IS NOT SAFE ON ITS OWN, and the eval breaks it. `sc-eval --repeat N`
//! runs the SAME command, in the same process on the same thread, against N freshly
//! copied workspaces; and two ladder rungs share the spelling `cargo test --offline -q`.
//! The workspace resets, the memory of what failed does not, so a brand-new fixture's
//! FIRST verification gets described as a delta against the previous run's final state.
//!
//! Measured on `rust-two-stage` x10 (2026-09-10): repeat 1 read
//! `run_verification: 1 failed, 5 passed:`, and every repeat after it was prefixed with
//! inherited state -- `same 1 failure as last run`, and on repeats 8 and 9 the baseline
//! of an untouched fixture claimed `now passing: padding_still_respects_a_later_component`.
//! The model is told, on turn zero, that it fixed and broke tests it has never seen.
//!
//! So a caller that starts a fresh workspace must say so: [`forget_runs`].

use std::cell::RefCell;
use std::collections::HashMap;

use crate::report::{compile_errors, TestReport};

thread_local! {
    /// command -> the failure signatures of its recent runs on this thread, oldest first.
    ///
    /// A HISTORY, not just the previous run. Comparing only against the immediately preceding
    /// verification makes an oscillation read as continuous progress: a model flipping between
    /// two states is told `now passing: X` on one turn and `newly failing: X` on the next,
    /// forever, because each is true *relative to the turn before*. Measured on
    /// `rust-symptomatic` x10 (2026-09-11): two runs alternated between two states for 27 and
    /// 19 consecutive turns and every single verification reported a win in one direction or
    /// the other. The labels actively reinforced the loop they should have exposed.
    static LAST_RUN: RefCell<HashMap<String, Vec<Vec<String>>>> = RefCell::new(HashMap::new());
}

/// How many past signatures per command are kept for the revisit check. Short on purpose --
/// this only has to catch a tight oscillation, and a long memory would call a legitimate
/// re-break of an old failure "revisited" many turns later.
const HISTORY: usize = 6;

/// Forget every remembered signature on this thread.
///
/// Call this when a FRESH workspace starts, so the first verification of new code is
/// reported on its own terms instead of as a delta against whatever ran here last. The
/// eval's `run_task` does it per task; a long-lived agent session never needs to, because
/// its workspace persists for the life of the run.
pub fn forget_runs() {
    LAST_RUN.with(|l| l.borrow_mut().clear());
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
    let (prev, revisited) = LAST_RUN.with(|l| {
        let mut map = l.borrow_mut();
        let hist = map.entry(command.to_string()).or_default();
        let prev = hist.last().cloned();
        // Has this exact state occurred BEFORE the previous run? That is an oscillation --
        // the workspace has come back to somewhere it has already been -- and it is the one
        // thing a one-step delta can never say.
        let revisited = hist.len() >= 2 && hist[..hist.len() - 1].contains(&now);
        hist.push(now.clone());
        if hist.len() > HISTORY {
            hist.remove(0);
        }
        (prev, revisited)
    });
    describe_run(&prev?, &now, ran_no_tests(report), revisited)
}

/// Did this run fail without running any tests at all -- i.e. the build broke?
///
/// A `generic` failed report is one the parser found no test lines in (`parse.rs`), which
/// for a compile failure means the suite never started. Its signature is `file:line`
/// compile errors, a vocabulary that cannot intersect the previous run's test names.
fn ran_no_tests(report: &TestReport) -> bool {
    report.generic && !report.command_ok
}

/// The delta clause between two signatures.
///
/// `now_is_a_build_failure` suppresses the `now passing` half: see
/// [`describe_run`]. Kept as the two-signature form for the tests that exercise
/// ordinary pass/fail transitions.
#[cfg(test)]
fn describe(prev: &[String], now: &[String]) -> Option<String> {
    describe_run(prev, now, false, false)
}

/// The delta clause between two signatures.
///
/// When `now_is_a_build_failure`, the `now passing` half is dropped. A test absent from a
/// broken build did not start passing -- it did not run. Reporting it as fixed told the
/// model its destructive edit was progress, which is precisely how `rust-ineffective-edit`
/// and `rust-shifting-anchor` ended with a model cheerfully rewriting a file it had just
/// destroyed. `newly failing` is kept: the compile errors are real and are what to fix.
fn describe_run(
    prev: &[String],
    now: &[String],
    now_is_a_build_failure: bool,
    revisited: bool,
) -> Option<String> {
    if prev == now {
        return match now.len() {
            0 => None,
            1 => Some("same 1 failure as last run".to_string()),
            n => Some(format!("same {n} failures as last run")),
        };
    }
    // The workspace is back somewhere it has already been. Say THAT, instead of dressing a
    // revert up as a win: `now passing: X` is true against the previous turn and profoundly
    // misleading across three, and a model reading it flips back and forth being congratulated
    // each way. This clause replaces the delta rather than joining it -- the delta is the part
    // that misleads.
    if revisited {
        return Some(match now.len() {
            0 => "this is a state you have already been in".to_string(),
            1 => "you are back to a state you have ALREADY TRIED (1 failure); \
                  undoing and redoing the same edit is not progress"
                .to_string(),
            n => format!(
                "you are back to a state you have ALREADY TRIED ({n} failures); \
                 undoing and redoing the same edit is not progress"
            ),
        });
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
    if !fixed.is_empty() && !now_is_a_build_failure {
        parts.push(format!("now passing: {}", list(&fixed)));
    }
    // Suppressing `now passing` can leave nothing to say. No clause beats an empty one:
    // `observation()` renders `Some(d)` as `{d} -- {body}`, so an empty string would reach
    // the model as a bare ` -- `.
    if parts.is_empty() {
        return None;
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

    /// THE LEAK. A fresh workspace must not inherit the last one's failure signature.
    ///
    /// `note_run` remembers per COMMAND, and the eval reruns one command against many
    /// freshly copied fixtures in a single process. Before `forget_runs`, the second
    /// fixture's very first verification was reported as a delta against the first
    /// fixture's last one -- so an untouched workspace was told what it had "fixed".
    #[test]
    fn a_fresh_workspace_does_not_inherit_the_previous_runs_signature() {
        let cmd = "sh test.sh --forget-runs-fixture";
        forget_runs();

        // Workspace one: two runs, so the second legitimately carries a delta.
        assert_eq!(
            note_run(cmd, &red(&["a", "b"])),
            None,
            "first run, no delta"
        );
        assert_eq!(
            note_run(cmd, &red(&["a"])).as_deref(),
            Some("now passing: b"),
            "within ONE workspace the delta is real and must survive"
        );

        // A new workspace begins. Its first verification is a first run, not a delta.
        forget_runs();
        assert_eq!(
            note_run(cmd, &red(&["a", "b"])),
            None,
            "a fresh fixture's baseline must be reported on its own terms"
        );
    }

    /// THE DEFECT: a test that vanished because the crate stopped compiling is reported
    /// as "now passing".
    ///
    /// A parsed run signs itself with TEST NAMES; a compile failure is `generic` and signs
    /// itself with `file:line` COMPILE ERRORS. The two vocabularies never intersect, so
    /// every test name from the previous run lands in `fixed` and is rendered as a win.
    ///
    /// Measured live on `rust-ineffective-edit`: the model replaced the whole of split.rs
    /// with a bare `for` loop, the file no longer compiled, ZERO tests ran, and the harness
    /// told it `now passing: a_short_label_past_an_hour_shows_the_minutes_within_that_hour,
    /// a_split_never_lets_a_field_overflow_its_unit, ... +2 more`. It had just destroyed the
    /// file and was told it had fixed five tests. Same shape on `rust-shifting-anchor`.
    #[test]
    fn a_compile_failure_never_reports_the_tests_it_stopped_running_as_passing() {
        let broke = TestReport::generic_with_output(
            false,
            "error[E0601]: `main` function not found\n --> split.rs:1\n",
        );
        // Through the production path -- `ran_no_tests` included, not a hardcoded flag.
        let clause = describe_run(
            &s(&["a_split_totals_back", "a_field_never_overflows"]),
            &signature(&broke),
            ran_no_tests(&broke),
            false,
        )
        .expect("a delta is still reported");
        assert!(
            !clause.contains("now passing"),
            "a build that does not compile ran no tests, so nothing can have started \
             passing -- got: {clause}"
        );
        assert!(
            clause.contains("newly failing") || clause.contains("split.rs:1"),
            "the compile error itself must still be surfaced: {clause}"
        );
    }

    /// **AN OSCILLATION MUST NOT BE LABELLED AS PROGRESS.**
    ///
    /// The delta compared only against the IMMEDIATELY PREVIOUS run, so a model flipping
    /// between two states was told `now passing: X` on one turn and `newly failing: X` on the
    /// next -- each true relative to the turn before, and together a lie about the run.
    /// Measured on `rust-symptomatic` x10 (2026-09-11): two runs alternated for 27 and 19
    /// consecutive turns and EVERY verification reported a win in one direction or the other.
    ///
    /// Driven through `note_run`, the production path, so the history bookkeeping is exercised
    /// rather than a hand-passed flag.
    #[test]
    fn returning_to_a_previous_state_is_reported_as_a_revisit_not_a_win() {
        let cmd = "sh test.sh --oscillation-fixture";
        forget_runs();

        // A -> B -> A: the third run is back where the first was.
        assert_eq!(note_run(cmd, &red(&["alpha"])), None, "first run, no delta");
        let to_b = note_run(cmd, &red(&["beta"])).expect("a delta");
        assert!(
            to_b.contains("now passing: alpha"),
            "a genuine one-step change still reads normally: {to_b}"
        );

        let back_to_a = note_run(cmd, &red(&["alpha"])).expect("a delta");
        assert!(
            back_to_a.contains("ALREADY TRIED"),
            "returning to a visited state must be named as such: {back_to_a}"
        );
        assert!(
            !back_to_a.contains("now passing"),
            "a revert must NOT be dressed up as a win: {back_to_a}"
        );
    }

    /// The guard against over-firing: a run that keeps producing NEW states is making
    /// progress, and must never be told it is going in circles.
    #[test]
    fn a_run_that_never_repeats_a_state_is_never_called_a_revisit() {
        let cmd = "sh test.sh --forward-progress";
        forget_runs();
        for names in [
            &["a", "b", "c"][..],
            &["a", "b"][..],
            &["a"][..],
            &["d"][..],
        ] {
            let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            if let Some(clause) = note_run(cmd, &red(&refs)) {
                assert!(
                    !clause.contains("ALREADY TRIED"),
                    "forward progress must not read as a loop: {clause}"
                );
            }
        }
    }

    /// The mirror case, so the fix cannot be "never say now passing". A genuine fix that
    /// takes a test from failing to passing must still be reported.
    #[test]
    fn a_real_fix_is_still_reported_as_now_passing() {
        let before = signature(&red(&["a", "b"]));
        let after = signature(&red(&["b"]));
        assert_eq!(
            describe(&before, &after).as_deref(),
            Some("now passing: a"),
            "a real pass-transition must survive the compile-failure guard"
        );
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
