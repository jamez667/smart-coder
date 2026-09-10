//! Point a failing assertion at the code that implements it.
//!
//! A verification failure names ONE location: the panic site. On a contract-test rung
//! that site is in the FROZEN test file, and the harness never named anything else --
//! so every observation the model received pointed at the one file it was forbidden to
//! edit. Measured on the `rust-two-stage` rung: `panicked at test.rs:36` was the only
//! file:line in 18 turns, the model anchored 14 edits on test.rs content (aiming them at
//! lib.rs, where they never matched), and it never once touched the buggy line. The
//! contract, the bug and the expected value were all in the prompt; nothing connected the
//! failing assertion to the source line that implements it.
//!
//! This resolves that last hop: read the asserting line, find the ONE symbol on it that
//! the index defines outside the panicking file, and name where it lives.
//!
//! CONSERVATIVE BY CONSTRUCTION. A wrong pointer is worse than no pointer -- it sends the
//! model to the wrong file, which is the exact failure this exists to fix -- so every
//! ambiguity declines. See [`hint_for`] for the full list.

use std::path::Path;

use sc_index::RepoIndex;

use crate::text::mentioned_identifiers;

/// Macros and other call-shaped tokens that are never the function under test. `assert_eq`
/// is on every asserting line in the corpus and resolves to nothing, but naming them
/// explicitly keeps a project that happens to DEFINE `assert_eq` from being pointed at it.
const NEVER: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "panic",
    "unwrap",
    "expect",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
];

/// The hint appended to a failing verification, or `None` when anything is ambiguous.
///
/// `failure` is the observation text carrying the panic (the parsed message from
/// [`sc_verify::TestReport`], which keeps the `panicked at <file>:<line>` header verbatim).
/// `workspace` is the run's root, read through the same [`RepoIndex`] the retrieval tools use.
///
/// Declines -- each returns `None`, silently:
///
/// * the panic location does not parse, or names a file outside the workspace;
/// * the asserting line cannot be read (the file moved, the line is past its end);
/// * NO identifier on that line resolves to an indexed definition;
/// * MORE THAN ONE distinct identifier resolves (which one is under test is a guess);
/// * the only resolutions are in the panicking file itself (a test helper -- pointing the
///   model back at the frozen file is the bug, not the fix);
/// * the symbol resolves into test code (`is_test`), or to several different files.
pub(super) fn hint_for(workspace: &Path, failure: &str) -> Option<String> {
    let index = RepoIndex::open(workspace);
    resolve(&index, failure)
}

/// The resolution itself, against an already-built index -- the seam the tests drive so
/// they never touch [`RepoIndex::open`]'s on-disk cache.
fn resolve(index: &RepoIndex, failure: &str) -> Option<String> {
    // The panic site. `resolve_trace` already parses every shape libtest emits (Windows
    // separators, the `(pid)` infix, `-q` mode) and maps the path into the workspace.
    let frame = sc_index::resolve_trace(failure, index)
        .into_iter()
        .find(|f| f.in_workspace() && f.line.is_some())?;
    let test_path = frame.path.clone()?;
    let test_line = frame.line?;

    // The source line that asserted. Read from the index's root so the path resolution
    // above is the ONLY path logic here.
    let source = std::fs::read_to_string(index.root().join(&test_path)).ok()?;
    let source = source.replace("\r\n", "\n");
    let line = source.lines().nth(test_line.checked_sub(1)?)?;

    // Every identifier on it that the index defines OUTSIDE the panicking file, in
    // non-test code. A helper defined in the frozen file (`fn v(..)` on the measured
    // rung) is excluded by the file check, not by luck: it has no `#[test]`, so the
    // index calls it production code, and pointing the model back at the frozen file is
    // precisely the failure being fixed.
    let mut found: Vec<(&str, &sc_index::IndexedSymbol, &str)> = Vec::new();
    for ident in mentioned_identifiers(line) {
        if NEVER.contains(&ident.as_str()) {
            continue;
        }
        let mut hits = definitions(index, &ident, &test_path);
        // One identifier defined in several files is not a resolution.
        if hits.len() != 1 {
            continue;
        }
        let (path, sym) = hits.remove(0);
        // `found` holds a borrow of the ident, so keep the owned string alive: look the
        // name up again off the symbol, which owns it.
        found.push((sym.name.as_str(), sym, path));
    }

    // Exactly one, or nothing. Two call-shaped names on one line and the function under
    // test is a guess -- and a guess is what this exists to remove.
    if found.len() != 1 {
        return None;
    }
    let (name, sym, path) = found[0];
    Some(format!(
        "The assertion at {test_path}:{test_line} calls `{name}()`, defined at {path}:{}-{}. \
         The fix goes there -- {test_path} is the test.",
        sym.line, sym.end_line
    ))
}

/// Every non-test definition of `name` outside `exclude`, as `(path, symbol)`.
fn definitions<'a>(
    index: &'a RepoIndex,
    name: &str,
    exclude: &str,
) -> Vec<(&'a str, &'a sc_index::IndexedSymbol)> {
    let mut out = Vec::new();
    for (path, rec) in &index.files {
        if path == exclude || rec.language.is_none() {
            continue;
        }
        for sym in &rec.symbols {
            if sym.name == name && !sym.is_test {
                out.push((path.as_str(), sym));
            }
        }
    }
    // Several definitions in ONE file (an overload, a duplicate name) is still ambiguous:
    // dedupe by path so the caller's `len() != 1` check sees the file count, then require
    // that the single file offered exactly one span.
    let distinct_paths = out
        .iter()
        .map(|(p, _)| *p)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if distinct_paths != 1 || out.len() != 1 {
        return Vec::new();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace on disk, indexed. The fixtures are small enough that a real index
    /// (rather than a hand-built one) keeps these tests honest about the actual
    /// extraction -- `is_test`, spans and all.
    fn repo(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-core-under-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            let p = dir.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    /// The `rust-two-stage` fixture, verbatim: the buggy comparator and its frozen
    /// contract test. The bug is `lib.rs:27`; the failing assertion is `test.rs:36`.
    const LIB: &str = "\
//! Version strings: parse and compare.

/// A dotted version, e.g. `1.4.12`.
#[derive(Debug, PartialEq, Eq)]
pub struct Version {
    pub parts: Vec<u32>,
}

/// Parse a dotted version. Returns `None` if any component is not a number.
pub fn parse(s: &str) -> Option<Version> {
    let mut parts = Vec::new();
    for p in s.split('.') {
        parts.push(p.parse::<u32>().ok()?);
    }
    Some(Version { parts })
}

/// Order two versions. Shorter versions are padded with zeros, so `1.4` == `1.4.0`.
pub fn compare(a: &Version, b: &Version) -> std::cmp::Ordering {
    // Compare component by component, longest wins on a tie.
    for (x, y) in a.parts.iter().zip(b.parts.iter()) {
        let ord = x.cmp(y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.parts.len().cmp(&b.parts.len())
}
";

    const TEST: &str = "\
// Contract test for version parse/compare. FROZEN: a solver must not modify this file.
#[path = \"lib.rs\"]
mod lib;
use lib::{compare, parse};
use std::cmp::Ordering;

fn v(s: &str) -> lib::Version {
    parse(s).expect(\"should parse\")
}

#[test]
fn parses_a_simple_version() {
    assert_eq!(v(\"1.4.12\").parts, vec![1, 4, 12]);
}

#[test]
fn rejects_a_non_numeric_component() {
    assert!(parse(\"1.x.3\").is_none());
}

#[test]
fn compares_component_by_component() {
    assert_eq!(compare(&v(\"1.4.0\"), &v(\"1.5.0\")), Ordering::Less);
    assert_eq!(compare(&v(\"2.0.0\"), &v(\"1.9.9\")), Ordering::Greater);
}

#[test]
fn numeric_not_lexicographic() {
    // 10 > 9, even though \"10\" sorts before \"9\" as text.
    assert_eq!(compare(&v(\"1.10.0\"), &v(\"1.9.0\")), Ordering::Greater);
}

#[test]
fn a_missing_component_counts_as_zero() {
    // Padding, not length: 1.4 and 1.4.0 are the SAME version.
    assert_eq!(compare(&v(\"1.4\"), &v(\"1.4.0\")), Ordering::Equal);
}

#[test]
fn padding_still_respects_a_later_component() {
    assert_eq!(compare(&v(\"1.4\"), &v(\"1.4.1\")), Ordering::Less);
}
";

    /// The exact failure the model was handed on the measured run.
    const LIVE_FAILURE: &str = "\
thread 'a_missing_component_counts_as_zero' panicked at test.rs:36:5:
assertion `left == right` failed
  left: Less
 right: Equal";

    /// THE LIVE CASE. The only location in the failure is the frozen test file; the hint
    /// must name lib.rs and the span of `compare`.
    #[test]
    fn names_the_function_under_test_not_the_frozen_test() {
        let ws = repo("live", &[("lib.rs", LIB), ("test.rs", TEST)]);
        let hint = hint_for(&ws, LIVE_FAILURE).expect("a hint for the live case");

        assert!(hint.contains("lib.rs:19-28"), "names the span: {hint}");
        assert!(hint.contains("`compare()`"), "names the function: {hint}");
        assert!(hint.contains("test.rs:36"), "names the assertion: {hint}");
        // The whole point: it points at editable code, and says the test is not it.
        assert!(hint.contains("The fix goes there"), "{hint}");
        // `v` is defined in the frozen file and must never be the answer.
        assert!(
            !hint.contains("`v()`"),
            "the test helper is not the fix: {hint}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The buggy line the model never touched (`lib.rs:27`) is inside the named span.
    #[test]
    fn the_named_span_contains_the_bug() {
        let ws = repo("span", &[("lib.rs", LIB), ("test.rs", TEST)]);
        let hint = hint_for(&ws, LIVE_FAILURE).unwrap();
        // 19-28 brackets line 27, where `a.parts.len().cmp(&b.parts.len())` lives.
        assert!(hint.contains("19-28"), "{hint}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// DECLINE: two resolvable symbols on the asserting line. Which is under test is a
    /// guess, and a guess is what this exists to remove.
    #[test]
    fn declines_when_the_line_is_ambiguous() {
        let lib = "\
pub fn alpha(x: u32) -> u32 {
    x
}
pub fn beta(x: u32) -> u32 {
    x
}
";
        let test = "\
#[path = \"lib.rs\"]
mod lib;
use lib::{alpha, beta};

#[test]
fn both() {
    assert_eq!(alpha(1), beta(2));
}
";
        let ws = repo("ambig", &[("lib.rs", lib), ("test.rs", test)]);
        let failure = "thread 'both' panicked at test.rs:7:5:\nassertion failed";
        assert_eq!(hint_for(&ws, failure), None, "two candidates => no pointer");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// DECLINE: the symbol is defined only in test code. There is no production site to
    /// point at, so saying nothing beats sending the model into a test file.
    #[test]
    fn declines_when_the_symbol_is_only_test_code() {
        let helpers = "\
#[cfg(test)]
mod helpers {
    pub fn only_in_tests(x: u32) -> u32 {
        x
    }
}
";
        let test = "\
#[test]
fn t() {
    assert_eq!(only_in_tests(1), 2);
}
";
        let ws = repo("testonly", &[("helpers.rs", helpers), ("test.rs", test)]);
        let failure = "thread 't' panicked at test.rs:3:5:\nassertion failed";
        assert_eq!(hint_for(&ws, failure), None, "test-only => no pointer");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// DECLINE: a helper defined in the PANICKING file. Pointing the model back at the
    /// frozen test is the exact failure being fixed.
    #[test]
    fn declines_when_the_only_symbol_is_in_the_test_file() {
        let test = "\
fn helper(x: u32) -> u32 {
    x
}

#[test]
fn t() {
    assert_eq!(helper(1), 2);
}
";
        let ws = repo("selffile", &[("test.rs", test)]);
        let failure = "thread 't' panicked at test.rs:7:5:\nassertion failed";
        assert_eq!(hint_for(&ws, failure), None, "same-file => no pointer");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// DECLINE: one name, two files. Which one implements the contract is a guess.
    #[test]
    fn declines_when_the_symbol_is_defined_in_two_files() {
        let a = "pub fn shared(x: u32) -> u32 {\n    x\n}\n";
        let b = "pub fn shared(x: u32) -> u32 {\n    x + 1\n}\n";
        let test = "\
#[test]
fn t() {
    assert_eq!(shared(1), 2);
}
";
        let ws = repo("twofiles", &[("a.rs", a), ("b.rs", b), ("test.rs", test)]);
        let failure = "thread 't' panicked at test.rs:3:5:\nassertion failed";
        assert_eq!(hint_for(&ws, failure), None, "two files => no pointer");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// DECLINE: no parseable panic location. Nothing to resolve from.
    #[test]
    fn declines_when_the_panic_does_not_parse() {
        let ws = repo("nopanic", &[("lib.rs", LIB), ("test.rs", TEST)]);
        assert_eq!(
            hint_for(&ws, "error: test failed, to rerun pass `--lib`"),
            None
        );
        assert_eq!(hint_for(&ws, ""), None);
        // A location outside the workspace resolves to no in-repo frame.
        assert_eq!(
            hint_for(
                &ws,
                "thread 'x' panicked at /rustc/deadbeef/library/core/src/slice.rs:117:5:"
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// DECLINE: the line number is past the end of the file (a stale report).
    #[test]
    fn declines_when_the_line_is_out_of_range() {
        let ws = repo("oob", &[("lib.rs", LIB), ("test.rs", TEST)]);
        let failure = "thread 't' panicked at test.rs:9000:5:\nassertion failed";
        assert_eq!(hint_for(&ws, failure), None);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Windows separators and the `(pid)` infix are the shapes real libtest emits.
    #[test]
    fn handles_the_windows_panic_shape() {
        let ws = repo("winshape", &[("lib.rs", LIB), ("test.rs", TEST)]);
        let failure = "thread 'a_missing_component_counts_as_zero' (64264) panicked at \
                       .\\test.rs:36:5:\nassertion `left == right` failed";
        let hint = hint_for(&ws, failure).expect("the windows shape still resolves");
        assert!(hint.contains("lib.rs:19-28"), "{hint}");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
