//! The gateway benchmark, run as a gate.
//!
//! Model-free and deterministic, so it belongs in `scripts/check.sh` beside the
//! retrieval eval rather than in a nightly job. A vocabulary or simplifier
//! change that makes routing worse fails the build with the need named.
//!
//! Run with `--nocapture` to see the full scorecard.

use std::path::PathBuf;

use sc_gateway::bench::{
    BenchSuite, CapturedResult, CapturedSuite, CaseResult, OutputResult, OutputSuite, Scorecard,
    Verdict,
};

/// The repo root. `CARGO_MANIFEST_DIR` is `crates/sc-gateway`; the suite lives
/// at the workspace root, the same layout the retrieval eval uses.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/sc-gateway has two ancestors")
        .to_path_buf()
}

fn suite() -> BenchSuite {
    BenchSuite::load(&repo_root().join("evals/gateway/suite.toml")).expect("suite loads")
}

#[test]
fn the_shipped_gateway_suite_passes() {
    let suite = suite();
    assert!(
        suite.cases.len() >= 15,
        "the suite should cover a real spread of needs, got {}",
        suite.cases.len()
    );

    let results = suite.run();
    let card = Scorecard::of(&results);

    println!("\n=== gateway benchmark ===");
    for r in &results {
        println!("{}", r.line());
    }
    println!("\n{}\n", card.summary());

    let failed: Vec<&CaseResult> = results.iter().filter(|r| !r.passed()).collect();
    if !failed.is_empty() {
        let report: Vec<String> = failed.iter().map(|r| r.line()).collect();
        panic!(
            "{} of {} gateway cases failed:\n{}",
            failed.len(),
            results.len(),
            report.join("\n")
        );
    }
}

#[test]
fn the_suite_is_deterministic() {
    // Without this the benchmark cannot be used as a gate: a score that moves on
    // its own makes every real regression arguable.
    let suite = suite();
    assert_eq!(suite.run(), suite.run());
}

#[test]
fn nothing_is_ever_misrouted() {
    // The single property the whole design exists to provide, asserted on its
    // own so a regression here is unmissable in the failure output. A misroute
    // hands the caller a confident answer from the wrong capability, with no
    // error to recover from — strictly worse than the refusals above it.
    let results = suite().run();
    let wrong: Vec<&CaseResult> = results
        .iter()
        .filter(|r| r.verdict == Verdict::Misrouted)
        .collect();
    assert!(
        wrong.is_empty(),
        "misrouted cases:\n{}",
        wrong
            .iter()
            .map(|r| r.line())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn every_declared_refusal_is_still_refused() {
    // The counterpart guard. These are needs the classifier promises to decline;
    // one starting to route means the table got looser, which is exactly the
    // drift that turns a careful router into a guessing one.
    let results = suite().run();
    let loose: Vec<&CaseResult> = results
        .iter()
        .filter(|r| r.verdict == Verdict::UnderRefused)
        .collect();
    assert!(
        loose.is_empty(),
        "needs that should have been refused but routed:\n{}",
        loose
            .iter()
            .map(|r| r.line())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_reduction_never_drops_a_required_fact() {
    // Reduction without this is gameable: returning nothing scores a perfect
    // 0% retained. A case that loses a `must_contain` string is a simplifier
    // that ate the answer, which is worse than not reducing at all.
    let results = suite().run();
    let lossy: Vec<&CaseResult> = results.iter().filter(|r| !r.missing.is_empty()).collect();
    assert!(
        lossy.is_empty(),
        "cases whose answer lost a required fact:\n{}",
        lossy
            .iter()
            .map(|r| r.line())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_suite_declares_both_routes_and_refusals() {
    // A suite of only-winnable needs measures nothing and quietly becomes a
    // suite the table was tuned against. Both kinds must be represented.
    let suite = suite();
    let refusals = suite
        .cases
        .iter()
        .filter(|c| c.expect.as_deref() == Some("refuse"))
        .count();
    let routes = suite.cases.len() - refusals;
    assert!(refusals >= 5, "too few declared refusals: {refusals}");
    assert!(routes >= 8, "too few routing cases: {routes}");
}

// ---------------------------------------------------------------------------
// The reduction benchmark: real captured cargo output, not hand-written mess.
// ---------------------------------------------------------------------------

fn output_suite() -> OutputSuite {
    OutputSuite::load(&repo_root().join("evals/gateway/output.toml")).expect("output suite loads")
}

#[test]
fn the_reduction_benchmark_meets_its_ceilings() {
    let results = output_suite().run();

    println!(
        "
=== reduction benchmark (real cargo output) ==="
    );
    for r in &results {
        println!("{}", r.line());
    }
    let raw: usize = results.iter().map(|r| r.raw_bytes).sum();
    let out: usize = results.iter().map(|r| r.out_bytes).sum();
    let pct = if raw == 0 {
        100
    } else {
        (out * 100).div_ceil(raw)
    };
    println!(
        "
TOTAL {raw} -> {out} bytes ({pct}% retained)
"
    );

    let failed: Vec<&OutputResult> = results.iter().filter(|r| !r.passed()).collect();
    assert!(
        failed.is_empty(),
        "{} of {} reduction cases failed:
{}",
        failed.len(),
        results.len(),
        failed.iter().map(|r| r.line()).collect::<Vec<_>>().join(
            "
"
        )
    );
}

#[test]
fn reduction_never_drops_a_required_fact_from_real_output() {
    // The guard that makes the percentage mean something. Shrinking is only a
    // win if what the caller needed survived.
    let results = output_suite().run();
    let lossy: Vec<&OutputResult> = results.iter().filter(|r| !r.missing.is_empty()).collect();
    assert!(
        lossy.is_empty(),
        "real output whose reduction lost a required fact:
{}",
        lossy.iter().map(|r| r.line()).collect::<Vec<_>>().join(
            "
"
        )
    );
}

#[test]
fn every_reduction_names_what_it_removed() {
    // Nothing is dropped silently: a capability that found nothing and a
    // simplifier that ate the answer must stay distinguishable after the fact.
    for r in output_suite().run() {
        if r.retained_percent() < 100 {
            assert!(
                !r.dropped.is_empty(),
                "{} shrank to {}% but named nothing it removed",
                r.id,
                r.retained_percent()
            );
        }
    }
}

#[test]
fn the_reduction_suite_is_deterministic() {
    let suite = output_suite();
    assert_eq!(suite.run(), suite.run());
}

// ---------------------------------------------------------------------------
// The captured-needs replay. The refusal rate, as a build gate.
// ---------------------------------------------------------------------------

fn captured() -> CapturedSuite {
    CapturedSuite::load(&repo_root().join("evals/gateway/captured.toml")).expect("capture loads")
}

#[test]
fn every_captured_need_is_handled() {
    let results = captured().run();

    println!(
        "
=== captured needs (verbatim from live A/B runs) ==="
    );
    for r in &results {
        println!("{}", r.line());
    }
    let ok = results.iter().filter(|r| r.passed()).count();
    println!(
        "
{ok}/{} handled
",
        results.len()
    );

    let failed: Vec<&CapturedResult> = results.iter().filter(|r| !r.passed()).collect();
    assert!(
        failed.is_empty(),
        "{} captured needs regressed:
{}",
        failed.len(),
        failed.iter().map(|r| r.line()).collect::<Vec<_>>().join(
            "
"
        )
    );
}

#[test]
fn no_captured_need_is_misrouted() {
    // A captured need routing to the WRONG capability is worse than the refusal
    // it replaced: the model gets a confident answer from the wrong source and
    // has no way to tell.
    let results = captured().run();
    let wrong: Vec<&CapturedResult> = results
        .iter()
        .filter(|r| r.verdict == Verdict::Misrouted)
        .collect();
    assert!(
        wrong.is_empty(),
        "misrouted captured needs:
{}",
        wrong.iter().map(|r| r.line()).collect::<Vec<_>>().join(
            "
"
        )
    );
}

#[test]
fn the_capture_keeps_its_declared_refusals() {
    // Some captured needs are genuinely ambiguous and SHOULD refuse. Keeping
    // them stops the file becoming a list of only-winnable cases that the table
    // was tuned against.
    let suite = captured();
    let refusals = suite.needs.iter().filter(|n| n.expect == "refuse").count();
    assert!(
        refusals >= 2,
        "the capture declares too few refusals: {refusals}"
    );
}
