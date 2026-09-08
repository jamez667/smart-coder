//! Router-only: does the classifier pick what a person would?
//!
//! **The check that found the router's real failure rate.** The 27-case
//! benchmark reported 100% because I wrote those cases in the vocabulary the
//! table already had. Asked the same questions in ordinary English, the router
//! scored 11/28 — sixteen refusals and a misroute.
//!
//! Every expectation below was written BEFORE running it, and two of them turned
//! out to be wrong about the gateway rather than the other way round: with no
//! file named, "show me handle_timeout" cannot be `code.function` (it needs a
//! path), so `code.symbol` is the correct answer.
//!
//! Refusals are listed explicitly rather than asserted away: they are the honest
//! remaining gaps, and a change that fixes one should show up here as news.

use sc_gateway::{classify, table, Need, Route};

/// Needs that must route, and where.
const MUST_ROUTE: &[(&str, &str)] = &[
    // The same intent, phrased many ways.
    ("read src/lib.rs", "file.read"),
    ("open src/lib.rs", "file.read"),
    ("cat src/lib.rs", "file.read"),
    ("what is in src/lib.rs", "file.read"),
    ("display src/lib.rs", "file.read"),
    ("give me src/lib.rs", "file.read"),
    // A bare path names its own intent: there is one thing to do with a file.
    ("src/lib.rs", "file.read"),
    ("what files are in src/agent", "file.list"),
    ("what's in the src/agent directory", "file.list"),
    ("ls src/agent", "file.list"),
    ("where is StallDetector", "code.symbol"),
    ("find the definition of StallDetector", "code.symbol"),
    ("which file defines StallDetector", "code.symbol"),
    // Names a function but no FILE, so a body read cannot run; the symbol can.
    ("print the handle_timeout function", "code.symbol"),
    // No file named, so a function BODY is not reachable — the symbol is.
    ("show me handle_timeout", "code.symbol"),
    ("what does handle_timeout do", "code.symbol"),
    ("grep for TODO", "code.search"),
    ("find TODO", "code.search"),
    ("locate StallDetector", "code.symbol"),
    ("which files mention TODO", "code.search"),
    ("run the tests", "verify.run"),
    ("what's failing", "verify.run"),
    ("are the tests green", "verify.run"),
    ("did anything break", "verify.run"),
    ("give me a tour of the codebase", "repo.map"),
    ("what crates does sc-core use", "cargo.info"),
    ("who depends on sc-tools", "cargo.info"),
];

/// Needs the router still cannot place. Recorded, not hidden: each is a real
/// gap, and one starting to route is news either way.
const STILL_REFUSED: &[&str] = &[
    // Names no operation and nothing structural: "project" is a hint, and
    // promoting it to a verb made "grep the codebase for StallDetector" tie
    // repo.map against code.search and refuse a plainly-stated grep. Refusing a
    // vague question costs the caller a turn; misrouting a clear one costs them
    // a wrong answer they cannot see, so this stays refused on purpose.
    //
    // "give me a tour of the codebase" routes, so the capability IS reachable —
    // this phrasing simply does not reach it.
    "what does this project do",
];

#[test]
fn ordinary_english_routes_where_a_person_would_expect() {
    let t = table();
    let mut wrong = Vec::new();
    for (need, want) in MUST_ROUTE {
        let got = match classify(&Need::new(*need), &t) {
            Route::To { index, .. } => t[index].name.to_string(),
            Route::Unknown { trace } => format!("REFUSED ({})", trace.reason),
        };
        if got != *want {
            wrong.push(format!(
                "  {need:?}
      got {got}, wanted {want}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} needs routed wrongly:
{}",
        wrong.len(),
        MUST_ROUTE.len(),
        wrong.join(
            "
"
        )
    );
}

#[test]
fn nothing_in_the_natural_phrasing_set_is_misrouted() {
    // Stricter than the test above and worth its own failure message: a REFUSAL
    // costs the caller a turn, a MISROUTE hands them a confident wrong answer
    // they cannot detect. Regressing into a misroute must never read as a small
    // change in a pass count.
    let t = table();
    let mut misrouted = Vec::new();
    for (need, want) in MUST_ROUTE {
        if let Route::To { index, .. } = classify(&Need::new(*need), &t) {
            if t[index].name != *want {
                misrouted.push(format!("  {need:?} -> {} (wanted {want})", t[index].name));
            }
        }
    }
    assert!(
        misrouted.is_empty(),
        "misrouted:
{}",
        misrouted.join(
            "
"
        )
    );
}

#[test]
fn the_recorded_gaps_are_still_gaps() {
    // If one of these starts routing, this test fails ON PURPOSE — move it into
    // MUST_ROUTE with the capability it now reaches. The alternative is a stale
    // list nobody revisits.
    let t = table();
    let mut resolved = Vec::new();
    for need in STILL_REFUSED {
        if let Route::To { index, .. } = classify(&Need::new(*need), &t) {
            resolved.push(format!("  {need:?} now routes to {}", t[index].name));
        }
    }
    assert!(
        resolved.is_empty(),
        "recorded gaps that now route — move them to MUST_ROUTE:
{}",
        resolved.join(
            "
"
        )
    );
}
