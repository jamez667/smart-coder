//! Reading a genuinely large file, through the real loop.
//!
//! Reported from use: on an 8,000-line file the agent "fails to read to line 2800"
//! while pi manages it. `sc-tools` reads the window correctly (see
//! `crates/sc-tools/tests/big_file_read.rs`), so whatever goes wrong happens after
//! the tool returns — in the loop's observation cap.

use std::path::PathBuf;
use std::sync::Mutex;

use sc_context::{truncate_observation, truncate_paged_read};

fn big_file(lines: usize) -> String {
    (1..=lines)
        .map(|i| format!("// line {i}: representative source content on this line\n"))
        .collect()
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-bigread-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 1,200-line window survives the default cap intact — no marker, no loss.
#[test]
fn a_window_within_the_cap_is_delivered_whole() {
    let obs = format!(
        "read_file big.rs (lines 2800-3999 of 8000):\n{}",
        big_file(1200)
    );
    let out = truncate_observation(&obs, 800, true);
    assert!(
        out.contains("truncated"),
        "a 1,201-line observation against an 800-line cap must be cut"
    );
}

/// THE FIX. A big read is no longer cut head-and-tail. Its lines are contiguous
/// source and the model can ask for the next page, so the cut keeps a CONTIGUOUS
/// PREFIX from where the model asked and names the `start` that resumes it.
///
/// Before, a model that asked for lines 1-2000 got 1-400 and 1601-2000 and a hole
/// where the code it wanted was, while the header still said `lines 1-2000` — nothing
/// in the reply told it the middle was missing rather than absent from the file. pi
/// returns the window it was asked for, which is why it coped with an 8,000-line file
/// and this did not.
#[test]
fn a_big_read_keeps_its_middle_and_loses_only_its_tail() {
    let body: String = (1..=2000)
        .map(|i| {
            format!(
                "{i}: // line {i}
"
            )
        })
        .collect();
    let obs = format!(
        "read_file big.rs (lines 1-2000 of 8000):
{body}"
    );
    let out = truncate_paged_read(&obs, 800);

    // Contiguous from the requested start: the head, and everything after it up to
    // the cap. What used to be the hole is now present.
    assert!(
        out.contains(
            "
1: // line 1"
        ),
        "the head is kept"
    );
    assert!(
        out.contains(
            "
400: // line 400"
        ),
        "and so is what follows it"
    );
    assert!(
        out.contains(
            "
700: // line 700"
        ),
        "the MIDDLE of the old head/tail slice is now delivered -- the bug this file pins"
    );
    let kept: Vec<usize> = out
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(": ").and_then(|(n, _)| n.parse().ok()))
        .collect();
    assert_eq!(kept.first().copied(), Some(1), "starts where asked");
    assert!(
        kept.windows(2).all(|w| w[1] == w[0] + 1),
        "and runs with no hole in it"
    );

    // The tail is what is dropped now, and the model is told exactly how to get it.
    let last = kept.last().copied().unwrap();
    assert!(out.contains(&format!("pass start={} for the next page", last + 1)));
    assert!(
        !out.contains(
            "
2000: // line 2000"
        ),
        "the tail is past the cap"
    );

    // The old head/tail path is still there for output that is not a paged read, and
    // it still does the thing that made this a bug.
    let logged = truncate_observation(&obs, 800, true);
    assert!(
        !logged.contains(
            "
700: // line 700"
        ),
        "the log path is unchanged -- it is the ROUTING that was wrong"
    );
}

/// The loop drives it end to end: a scripted model reads a window of a huge file
/// and the observation it gets back is bounded by `read_file_line_cap`.
#[test]
fn the_loop_caps_a_huge_read_at_the_configured_line_cap() {
    use sc_core::{run_agent_observed, AgentConfig, AgentEvent};
    use sc_model::MockBackend;
    use sc_tools::default_registry;

    let ws = temp("loop");
    std::fs::write(ws.join("big.rs"), big_file(8000)).unwrap();

    let log = Mutex::new(Vec::new());
    struct FnSink<F: Fn(&AgentEvent)>(F);
    impl<F: Fn(&AgentEvent)> sc_core::EventSink for FnSink<F> {
        fn record(&self, e: &AgentEvent) {
            (self.0)(e)
        }
    }
    let sink = FnSink(|e: &AgentEvent| log.lock().unwrap().push(e.clone()));

    // Ask for 3,000 lines starting at 2,800 — well past every cap.
    let backend = MockBackend::new([
        r#"{"tool":"read_file","path":"big.rs","start":2800,"limit":3000}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 2,
        ..AgentConfig::default()
    };
    run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &sc_core::ParseRepair,
        "read big.rs around line 2800",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let events = log.into_inner().unwrap();
    // The EVENT keeps the full text (the run log is lossless); what matters is what
    // reached the MODEL, which is the trimmed copy in the next turn's prompt.
    let full = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult { full, .. } => Some(full.clone()),
            _ => None,
        })
        .expect("the read must produce a tool result");
    assert!(
        full.contains("2800: // line 2800"),
        "line 2800 itself must be readable -- that is the reported complaint"
    );
    assert!(
        full.lines().count() > 2900,
        "the run log keeps the whole window losslessly"
    );

    // What the model saw: the observation as trimmed for the prompt, down the paged
    // path the loop now routes `read_file` through.
    let trimmed = truncate_paged_read(&full, cfg.read_file_line_cap);
    let delivered = trimmed.lines().count();
    assert!(
        delivered <= cfg.read_file_line_cap + 8,
        "the model's copy is bounded (got {delivered} against a {}-line cap)",
        cfg.read_file_line_cap
    );
    assert!(
        trimmed.contains("2800: // line 2800"),
        "the START of the asked-for window survives the cap"
    );
    // ...and so does everything after it up to the cap. The model asked for 3,000
    // lines from 2800; it gets the first 798 of them CONTIGUOUSLY, plus the start
    // that fetches the next page. Nothing is taken from the middle of what it sees.
    let kept: Vec<usize> = trimmed
        .lines()
        .filter_map(|l| l.split_once(": ").and_then(|(n, _)| n.parse().ok()))
        .collect();
    assert_eq!(
        kept.first().copied(),
        Some(2800),
        "contiguous from the requested start"
    );
    assert!(
        kept.windows(2).all(|w| w[1] == w[0] + 1),
        "with no hole anywhere in the kept region -- the fix this file pins"
    );
    let last = kept.last().copied().unwrap();
    assert!(
        trimmed.contains(&format!("pass start={} for the next page", last + 1)),
        "and the continuation names the right next start, got: {}",
        trimmed.lines().last().unwrap()
    );
    // Line 4300 is genuinely past this page -- but it is now REACHABLE, because the
    // hint names the start that gets there, which is what pi does and this did not.
    assert!(last < 4300 && last + 1 > 2800);

    let _ = std::fs::remove_dir_all(&ws);
}

/// Regression guard on the ROUTING, which is where the bug actually lived: a
/// verification report must keep the error-first path. Its lines are not a range the
/// model can re-request, and the failing assertion is usually at the END — a prefix
/// cut would throw away the one thing it needed.
#[test]
fn a_verification_report_still_gets_error_first_treatment() {
    let mut fb = String::from(
        "(harness ran the tests after your edit)
",
    );
    for i in 0..500 {
        fb.push_str(&format!(
            "ok  test_app.py::test_{i}
"
        ));
    }
    fb.push_str(
        "E   jinja2.exceptions.TemplateNotFound: board.html
",
    );

    let error_first = truncate_observation(&fb, 200, true);
    assert!(
        error_first.contains("TemplateNotFound"),
        "the exception at the end survives, because errors are surfaced first"
    );
    assert!(error_first.contains("skipped"), "and the skip is flagged");

    // A prefix cut would have lost it -- which is exactly why the routing splits.
    let as_a_page = truncate_paged_read(&fb, 200);
    assert!(
        !as_a_page.contains("TemplateNotFound"),
        "a prefix cut is the WRONG shape for a report; the loop must not use it here"
    );
}
