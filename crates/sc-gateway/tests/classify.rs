//! Routing tests — the classifier under adversarial pressure.
//!
//! The bar these hold: **a wrong route must be impossible to reach quietly**.
//! Every test here is either "routes to the obviously right thing" or "refuses
//! rather than guessing". A test that asserts a *plausible-but-wrong* route
//! passes is a bug in the test.

use sc_gateway::{classify, table, Class, Gateway, Need, Route, AMBIGUITY_MARGIN, MIN_SCORE};

/// Route a need, returning the capability name or None for a refusal.
fn route_of(text: &str) -> Option<String> {
    let t = table();
    match classify(&Need::new(text), &t) {
        Route::To { index, .. } => Some(t[index].name.to_string()),
        Route::Unknown { .. } => None,
    }
}

fn route_scoped(text: &str, scope: &str) -> Option<String> {
    let t = table();
    match classify(&Need::scoped(text, scope), &t) {
        Route::To { index, .. } => Some(t[index].name.to_string()),
        Route::Unknown { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// The happy path: needs a caller would actually write.
// ---------------------------------------------------------------------------

#[test]
fn reads_a_named_file() {
    assert_eq!(route_of("read src/main.rs").as_deref(), Some("file.read"));
    assert_eq!(
        route_of("show me the contents of Cargo.toml").as_deref(),
        Some("file.read")
    );
}

#[test]
fn lists_a_directory() {
    assert_eq!(
        route_scoped("list the files in that folder", "crates/").as_deref(),
        Some("file.list")
    );
}

#[test]
fn searches_for_literal_text() {
    assert_eq!(
        route_of("grep for the string TODO across the workspace").as_deref(),
        Some("code.search")
    );
}

#[test]
fn finds_a_symbol_definition() {
    assert_eq!(
        route_of("where is the struct ToolRegistry declared").as_deref(),
        Some("code.symbol")
    );
}

#[test]
fn asks_for_a_repo_overview() {
    assert_eq!(
        route_of("give me an overview of this codebase").as_deref(),
        Some("repo.map")
    );
}

#[test]
fn routes_open_questions_to_the_live_model() {
    assert_eq!(
        route_of("explain why this design uses a trait").as_deref(),
        Some("reason.explain")
    );
}

#[test]
fn routes_outward_questions_to_web() {
    assert_eq!(
        route_of("what is the latest release of the serde documentation online").as_deref(),
        Some("web.search")
    );
}

#[test]
fn routes_crate_questions_to_the_manifests() {
    assert_eq!(
        route_of("what does the sc-proto crate depend on").as_deref(),
        Some("cargo.info")
    );
}

#[test]
fn routes_performance_questions_to_the_profile() {
    assert_eq!(
        route_scoped("what are the hotspots", "target/p.folded").as_deref(),
        Some("perf.hotspots")
    );
}

#[test]
fn routes_suite_questions_to_verification() {
    assert_eq!(route_of("run the tests").as_deref(), Some("verify.run"));
    assert_eq!(
        route_of("which tests are failing").as_deref(),
        Some("verify.run")
    );
}

#[test]
fn a_crate_question_never_goes_to_the_network() {
    // "crates" names the local manifests, not crates.io. Sending a free local
    // lookup over the network would be the expensive kind of misroute.
    //
    // Note "list the crates in this workspace" deliberately does NOT assert
    // cargo.info: `list` and `crates` are one verb each, so it is a real tie
    // between listing directories and listing crates, and a refusal is the
    // honest answer. What must never happen is web.search.
    for need in [
        "what does the sc-proto crate depend on",
        "list the crates in this workspace",
        "which crates depend on sc-tools",
    ] {
        assert_ne!(
            route_of(need).as_deref(),
            Some("web.search"),
            "{need:?} was routed to the network"
        );
    }
}

#[test]
fn a_hint_advantage_breaks_a_tie_when_the_verbs_are_unequal() {
    // The tie-breaker: "what does the sc-proto crate depend on" matches TWO
    // cargo.info verbs (crate, depend), so it wins outright over any
    // single-verb rival rather than refusing on a narrow point margin.
    assert_eq!(
        route_of("what does the sc-proto crate depend on").as_deref(),
        Some("cargo.info")
    );
}

// ---------------------------------------------------------------------------
// Refusals. Each of these is a need that a naive router would answer WRONGLY,
// and answering wrongly is undetectable downstream — so the only safe outcome
// is a refusal.
// ---------------------------------------------------------------------------

#[test]
fn refuses_an_empty_need() {
    assert_eq!(route_of(""), None);
}

#[test]
fn refuses_pure_noise() {
    assert_eq!(route_of("hello there how are you"), None);
    assert_eq!(route_of("do the thing"), None);
}

#[test]
fn refuses_a_read_with_no_path() {
    // "read the file" names no file. Answering it means reading *something*
    // arbitrary and presenting it as the answer.
    assert_eq!(route_of("read the file"), None);
    assert_eq!(route_of("show me the source"), None);
}

#[test]
fn a_scope_satisfies_the_path_requirement() {
    assert_eq!(
        route_scoped("read the file", "src/lib.rs").as_deref(),
        Some("file.read")
    );
}

#[test]
fn refuses_when_a_single_hint_is_the_only_evidence() {
    // "file" is a hint (3), well under MIN_SCORE. A noun is not an instruction.
    let t = table();
    match classify(&Need::new("file"), &t) {
        Route::Unknown { trace } => {
            assert!(
                trace.reason.contains("below threshold") || trace.reason.contains("no capability"),
                "unexpected refusal reason: {}",
                trace.reason
            );
        }
        Route::To { index, .. } => panic!("routed a bare hint to {}", t[index].name),
    }
}

#[test]
fn refuses_a_genuine_tie_rather_than_picking_by_noise() {
    // Both code.search and code.symbol are plausible readings of this. The
    // scores land within the ambiguity margin, so it must refuse.
    let t = table();
    let need = Need::new("find the definition");
    match classify(&need, &t) {
        Route::Unknown { .. } => {}
        Route::To { index, .. } => {
            // Permitted only if the winner was genuinely decisive.
            let lower = need.text.to_lowercase();
            let mut scores: Vec<u32> = t.iter().map(|c| sc_gateway::score(c, &lower)).collect();
            scores.sort_unstable_by(|a, b| b.cmp(a));
            assert!(
                scores[0] - scores[1] >= AMBIGUITY_MARGIN,
                "routed to {} on a {}-point margin",
                t[index].name,
                scores[0] - scores[1]
            );
        }
    }
}

#[test]
fn the_refusal_names_what_it_considered() {
    // A refusal the caller cannot act on is only marginally better than a
    // misroute. Every refusal carries its candidate list and a reason.
    let t = table();
    match classify(&Need::new("read the file"), &t) {
        Route::Unknown { trace } => {
            assert!(!trace.reason.is_empty(), "refusal with no reason");
            assert!(trace.route.is_none());
            assert!(trace.class.is_none());
        }
        Route::To { .. } => panic!("should have refused"),
    }
}

// ---------------------------------------------------------------------------
// Word-boundary traps: substring matching produced real misroutes here.
// ---------------------------------------------------------------------------

#[test]
fn substrings_do_not_trigger_a_route() {
    // "already" contains "read"; "direction" contains "dir"; "define" contains
    // no whole verb. None of these should route.
    assert_eq!(route_of("already done"), None);
    assert_eq!(route_of("the direction of travel"), None);
}

#[test]
fn a_tool_name_typed_as_prose_still_routes() {
    // **Reversed after the A/B.** This test used to assert the opposite: that
    // `read_file` must NOT match the verb `read`, on the theory that an
    // identifier is one token. That rule refused the single clearest request in
    // the whole A/B log — a model typing "read_file pathfind.rs lines 30-114",
    // which scored 3 and got "Cannot answer that".
    //
    // A model that has seen a tool surface types its names as prose. Splitting
    // on underscores routes those, and the word-boundary rule below still stops
    // the false positives that motivated the original test.
    let t = table();
    let read_cap = t.iter().find(|c| c.name == "file.read").unwrap();
    assert!(
        sc_gateway::score(read_cap, "read_file pathfind.rs lines 30-114") >= MIN_SCORE,
        "a tool name typed as prose no longer routes"
    );
}

#[test]
fn splitting_identifiers_does_not_resurrect_substring_matching() {
    // The guard the reversal above must not weaken: `already` contains `read`
    // but is one word, so it must still score nothing. Underscore splitting is
    // narrower than substring matching, and this pins the difference.
    let t = table();
    let read_cap = t.iter().find(|c| c.name == "file.read").unwrap();
    assert_eq!(sc_gateway::score(read_cap, "already done"), 0);
    assert_eq!(sc_gateway::score(read_cap, "spreading the load"), 0);
}

// ---------------------------------------------------------------------------
// Table invariants. These catch a capability added carelessly later.
// ---------------------------------------------------------------------------

#[test]
fn every_capability_has_a_distinct_name() {
    let t = table();
    let mut names: Vec<&str> = t.iter().map(|c| c.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "duplicate capability name in the table"
    );
}

#[test]
fn every_capability_declares_at_least_one_verb() {
    // A capability with no verbs can never clear MIN_SCORE — it is unreachable
    // dead weight that still costs the router a comparison.
    for cap in table() {
        assert!(
            !cap.verbs.is_empty(),
            "{} declares no verbs and is unreachable",
            cap.name
        );
    }
}

#[test]
fn no_verb_is_claimed_by_two_capabilities() {
    // A shared verb is how two capabilities land inside the ambiguity margin on
    // ordinary phrasing — which turns real needs into refusals.
    let t = table();
    for (i, a) in t.iter().enumerate() {
        for b in t.iter().skip(i + 1) {
            for verb in a.verbs {
                assert!(
                    !b.verbs.contains(verb),
                    "verb {verb:?} is claimed by both {} and {}",
                    a.name,
                    b.name
                );
            }
        }
    }
}

#[test]
fn a_verb_on_one_capability_is_not_a_hint_on_another() {
    // The subtler collision, and the one that actually bit: "show" as a verb on
    // file.read and a hint on code.function put them 3 points apart on ordinary
    // phrasing — inside the ambiguity margin, so a real need became a refusal.
    // Distinct verb sets alone do not prevent this; the vocabularies must not
    // overlap at all.
    let t = table();
    for a in &t {
        for b in &t {
            if a.name == b.name {
                continue;
            }
            for verb in a.verbs {
                assert!(
                    !b.hints.contains(verb),
                    "{verb:?} is a verb on {} and a hint on {} — they will tie",
                    a.name,
                    b.name
                );
            }
        }
    }
}

#[test]
fn min_score_is_reachable_by_a_single_verb() {
    // The threshold must stay tuned to the weights, or nothing routes at all.
    // Asserted behaviourally: every capability must be reachable by its own
    // first verb plus a path, which is the least a caller could type.
    let t = table();
    for cap in &t {
        let verb = cap.verbs.first().expect("verbs checked elsewhere");
        let text = if cap.needs_path {
            format!("{verb} src/lib.rs")
        } else {
            format!("{verb} handle_timeout")
        };
        let lower = text.to_lowercase();
        assert!(
            sc_gateway::score(cap, &lower) >= MIN_SCORE,
            "{} is unreachable by its own verb {verb:?}",
            cap.name
        );
    }
}

#[test]
fn free_and_repeatable_agree_with_the_class_split() {
    assert!(Class::Deterministic.free() && Class::Deterministic.repeatable());
    assert!(!Class::Retrieval.free() && Class::Retrieval.repeatable());
    assert!(!Class::Live.free() && !Class::Live.repeatable());
    assert!(!Class::Web.free() && !Class::Web.repeatable());
}

// ---------------------------------------------------------------------------
// Availability: a capability whose seam is missing refuses; it never silently
// falls through to a different capability that would answer confidently.
// ---------------------------------------------------------------------------

#[test]
fn live_capability_refuses_with_no_model_configured() {
    let gw = Gateway::new();
    let answer = gw.ask(
        &Need::new("explain why this exists"),
        std::path::Path::new("."),
    );
    assert!(answer.is_unknown(), "answered a live need with no model");
    assert!(
        answer.trace.reason.contains("unavailable"),
        "unexpected reason: {}",
        answer.trace.reason
    );
}

#[test]
fn web_capability_refuses_with_no_web_configured() {
    let gw = Gateway::new();
    let answer = gw.ask(
        &Need::new("check the latest version online"),
        std::path::Path::new("."),
    );
    assert!(answer.is_unknown());
}

#[test]
fn a_refusal_reads_as_a_refusal_to_the_model() {
    // The model only ever sees `text`. A refusal must be unmistakable there,
    // not just in the trace it never sees.
    let gw = Gateway::new();
    let answer = gw.ask(&Need::new("do something vague"), std::path::Path::new("."));
    assert!(
        answer.text.starts_with("Cannot answer that"),
        "refusal text was not self-evident: {}",
        answer.text
    );
}
