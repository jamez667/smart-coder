//! The **scoped completion check** (spec 08 step 1): is THIS subtask done, as
//! distinct from "did the merge make the suite worse?".
//!
//! The cumulative integration gate only asks the second question, and a worker's
//! *partial* fix can keep the failing count flat, integrate, and leave the board
//! all-done over a still-red suite. These helpers answer the first, scoping the
//! verify command to the subtask's own contract tests where they are known and
//! falling back to a whole-suite delta where they are not.

use std::path::Path;

/// The subtask's residual failing tests after a merge — the *scoped* completion
/// check (spec 08 step 1). Two modes:
///
/// - **Frozen tests known** (staged workflow): run the verify command **filtered to
///   the frozen contract-test paths** (`pytest <those files>`); the failing cases it
///   reports are the subtask's own unmet contract. Precise.
/// - **Frozen tests unknown** (free-text `swarm <task>`, `frozen_paths` empty): fall
///   back to the **whole-suite delta vs. this subtask's baseline** — incomplete iff
///   the suite is still red AND this subtask's merge didn't clear it. Coarser (can't
///   attribute a residual to one subtask), but stops a red run being called done.
pub(super) fn scoped_failures(
    sandbox: &sc_verify::Sandbox,
    workspace: &Path,
    verify_command: &str,
    frozen: &[String],
    baseline: Option<usize>,
) -> Vec<sc_verify::TestCase> {
    if frozen.is_empty() {
        // Free-text fallback: whole-suite delta vs. this subtask's own baseline.
        let report = sc_verify::run_verification_in(sandbox, workspace, verify_command);
        let after = badness(&report);
        let still_red = after > 0;
        let cleared = baseline.map(|b| after < b).unwrap_or(false);
        if still_red && !cleared {
            let failed: Vec<sc_verify::TestCase> = report.failed().into_iter().cloned().collect();
            if failed.is_empty() {
                vec![synthetic_failure("suite still red after this subtask")]
            } else {
                failed
            }
        } else {
            Vec::new()
        }
    } else {
        // Precise: verify filtered to this subtask's frozen contract tests. Tests can
        // span languages (Python backend + JS frontend), so run each group with its OWN
        // runner — pytest for `.py`, vitest for `.test.js` — and combine the failures.
        // (A single `pytest test_x.js` can't run JS; that mismatch reverted every
        // frontend subtask, observed live 2026-06-14.)
        let mut py: Vec<&String> = Vec::new();
        let mut js: Vec<&String> = Vec::new();
        for f in frozen {
            if is_js_test(f) {
                js.push(f);
            } else {
                py.push(f);
            }
        }
        let mut residual = Vec::new();
        if !py.is_empty() {
            let cmd = format!(
                "{verify_command} {}",
                py.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
            );
            residual.extend(run_scoped(sandbox, workspace, &cmd));
        }
        if !js.is_empty() {
            // Vitest filters by a path substring; pass each file. The container has it.
            let cmd = format!(
                "vitest run {}",
                js.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
            );
            residual.extend(run_scoped(sandbox, workspace, &cmd));
        }
        residual
    }
}

/// Whether a test path is a JS test (vitest's `*.test.js`/`*.spec.js` convention),
/// vs a Python `test_*.py` (pytest).
pub(super) fn is_js_test(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".test.js") || l.ends_with(".spec.js") || l.ends_with(".test.mjs")
}

/// Run one scoped verify command and return its failing cases (or a synthetic failure
/// when the runner errored with no per-test detail).
pub(super) fn run_scoped(
    sandbox: &sc_verify::Sandbox,
    workspace: &Path,
    cmd: &str,
) -> Vec<sc_verify::TestCase> {
    let report = sc_verify::run_verification_in(sandbox, workspace, cmd);
    if report.generic {
        if report.command_ok {
            Vec::new()
        } else {
            vec![synthetic_failure(
                "scoped tests failed (no per-test detail)",
            )]
        }
    } else {
        report.failed().into_iter().cloned().collect()
    }
}

/// A stand-in failing case for paths where we know the subtask is incomplete but have
/// no per-test breakdown (generic/exit-code-only suites, rejected merges).
pub(super) fn synthetic_failure(msg: &str) -> sc_verify::TestCase {
    sc_verify::TestCase {
        name: msg.to_string(),
        passed: false,
        message: None,
    }
}

/// The feedback block for a retry prompt: still-failing test names + their assertion
/// messages (spec 08 — `TestReport::failed()` carries `name` + `message`).
pub(super) fn feedback_text(residual: &[sc_verify::TestCase]) -> String {
    let mut s = String::new();
    for c in residual {
        s.push_str(&format!("✗ {}", c.name));
        if let Some(m) = &c.message {
            s.push_str(&format!("\n    {}", m.replace('\n', "\n    ")));
        }
        s.push('\n');
    }
    s.trim_end().to_string()
}

/// How "bad" a verification result is, comparable before vs after a change. For a
/// parsed report it's the number of failing tests, plus one if the command itself
/// errored with no failures parsed (e.g. a pytest *collection* error from a broken
/// import — green-looking to a naive failed-count but actually a hard failure). For
/// a generic (exit-code-only) report it's 0 if the command passed, else 1. This
/// lets the cumulative gate ("don't make it worse") work for both pytest-style and
/// bare-shell suites and never mistake a collection error for success.
pub(super) fn badness(report: &sc_verify::TestReport) -> usize {
    if report.generic {
        usize::from(!report.command_ok)
    } else {
        let failures = report.failed().len();
        // A non-zero exit with zero parsed failures means the suite didn't even run
        // (import/collection error) — count it as bad so the gate won't accept it.
        failures + usize::from(failures == 0 && !report.command_ok)
    }
}

/// Is `path` one of the frozen contract-test paths? Compared with normalized
/// separators so `tests/a.py` and `tests\a.py` match.
pub(super) fn is_frozen(path: &str, frozen: &[String]) -> bool {
    let norm = |s: &str| s.replace('\\', "/");
    let p = norm(path);
    frozen.iter().any(|f| norm(f) == p)
}

/// The frozen test files that belong to a subtask's `source_files` — its own contract,
/// for the scoped completion check. Tests are 1:1 with source files by basename stem:
/// `app.py` → `test_app.py`, `index.html` → `index.test.js`. A subtask is judged only by
/// these, not the whole suite (tests for other, not-yet-written files would keep it red).
pub(super) fn own_tests(source_files: &[String], frozen: &[String]) -> Vec<String> {
    let stem = |path: &str| -> String {
        let base = path.replace('\\', "/");
        let base = base.rsplit('/').next().unwrap_or(&base);
        // The bare name: drop every extension and a leading `test_`.
        let s = base.split('.').next().unwrap_or(base);
        s.strip_prefix("test_").unwrap_or(s).to_ascii_lowercase()
    };
    let want: std::collections::HashSet<String> = source_files.iter().map(|f| stem(f)).collect();
    frozen
        .iter()
        .filter(|t| want.contains(&stem(t)))
        .cloned()
        .collect()
}
