//! End-to-end tests: a real workspace on disk, through the whole gateway.
//!
//! These are the ones that would catch a stage wired to the wrong thing —
//! routing that works in isolation but hands the executor an argument it cannot
//! use, or a simplifier that runs before extraction. The unit tests cover each
//! stage; these cover the seams between them.

use std::fs;
use std::path::Path;

use sc_gateway::{Ctx, Gateway, Need, WebSearch};

/// A small but realistic workspace: nested dirs, a couple of Rust files with
/// real symbols, and a file whose name is easy to confuse with another.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::create_dir_all(root.join("src/agent")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod agent;\n\n/// The entry point.\npub fn run_demo() -> usize {\n    42\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/agent/stall.rs"),
        "/// Detect a stalled agent loop.\npub fn handle_timeout(ticks: usize) -> bool {\n    ticks > 3\n}\n\npub struct StallDetector {\n    pub ticks: usize,\n}\n",
    )
    .unwrap();
    dir
}

fn ask(gw: &Gateway, need: &Need, root: &Path) -> sc_gateway::Answer {
    gw.ask(need, root)
}

// ---------------------------------------------------------------------------
// Each capability, end to end, against real files.
// ---------------------------------------------------------------------------

#[test]
fn reads_a_real_file_through_the_registry() {
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("read src/lib.rs"), dir.path());

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(answer.text.contains("run_demo"), "got: {}", answer.text);
    assert_eq!(answer.trace.route.as_deref(), Some("file.read"));
}

#[test]
fn a_scope_beats_a_path_guessed_from_the_text() {
    // The scope is authoritative — it came from the harness, not the model.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("read the file", "src/agent/stall.rs"),
        dir.path(),
    );
    assert!(
        answer.text.contains("handle_timeout"),
        "got: {}",
        answer.text
    );
}

#[test]
fn lists_a_real_directory() {
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("list the files there", "src"),
        dir.path(),
    );

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(answer.text.contains("lib.rs"), "got: {}", answer.text);
}

#[test]
fn a_search_query_keeps_only_the_identifier() {
    // **Found by the direct-vs-gateway comparison.** "please find all
    // occurrences of handle_timeout in the codebase" leaked the word "codebase"
    // into the query and matched NOTHING, where the direct tool found the
    // symbol. The old fix would have been to add "codebase" to a blocklist,
    // which just moves the boundary to the next unlisted word — so the query is
    // now built from the identifier-shaped words instead.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::new("please find all occurrences of handle_timeout in the codebase"),
        dir.path(),
    );

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        answer.text.contains("stall.rs"),
        "prose leaked into the query: {}",
        answer.text
    );
    assert!(
        !answer.text.contains("no matches"),
        "search found nothing: {}",
        answer.text
    );
}

#[test]
fn a_prose_search_without_an_identifier_still_works() {
    // The counterpart: a search for a phrase has no identifier to latch onto,
    // so the blocklist path must survive for it.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::new("search the codebase for the phrase entry point"),
        dir.path(),
    );
    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        !answer.text.contains("codebase"),
        "filler leaked into a prose query: {}",
        answer.text
    );
}

#[test]
fn a_plain_lowercase_function_name_is_found() {
    // **Found by running the direct tools and the gateway side by side on the
    // same needs.** `symbol_term` accepted only snake_case/CamelCase/fn()
    // shapes, so "the body of the classify function" — whose target is a plain
    // lowercase word — yielded "no function name found in the need" while a
    // direct `read_function` returned the function. Position beats shape: a
    // word beside "function" has been named, whatever its capitalisation.
    let dir = workspace();
    std::fs::write(
        dir.path().join("src/plain.rs"),
        "pub fn classify(x: usize) -> bool {
    x > 1
}

pub fn other() {}
",
    )
    .unwrap();

    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("show me the body of the classify function", "src/plain.rs"),
        dir.path(),
    );

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        answer.text.contains("classify"),
        "did not find the function: {}",
        answer.text
    );
    assert!(
        !answer.text.contains("no function name found"),
        "still rejecting a lowercase name: {}",
        answer.text
    );
}

#[test]
fn a_directory_named_in_the_text_is_used_not_the_root() {
    // **Found by running the gateway against this repository rather than a
    // fixture.** `file.list` read only `need.scope` and fell back to "."
    // unconditionally, so "list the files in crates/sc-gateway/src" listed the
    // REPOSITORY ROOT and said nothing about having done so — a confident wrong
    // answer, which is the failure class this whole design exists to prevent.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("list the files in src/agent"), dir.path());

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        answer.text.contains("stall.rs"),
        "did not list the named directory: {}",
        answer.text
    );
    // The tell for the old bug: the root holds Cargo.toml, the named dir does not.
    assert!(
        !answer.text.contains("Cargo.toml"),
        "listed the workspace root instead of the named directory: {}",
        answer.text
    );
}

#[test]
fn an_unscoped_list_refuses_rather_than_guessing_a_directory() {
    // The counterpart, and it surprised me: `file.list` declares `needs_path`,
    // so a bare "list the files" REFUSES instead of quietly listing the working
    // directory. That is right — the same rule as "read the file" — and it means
    // the executor's "." fallback is only reachable via an explicit scope.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("list the files"), dir.path());
    assert!(
        answer.is_unknown(),
        "guessed a directory for an unscoped list: {}",
        answer.text
    );
}

#[test]
fn searches_real_content_and_strips_the_instruction_words() {
    // The need is conversational; the underlying tool needs the bare term. If
    // the noise words leaked through, this finds nothing.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::new("search for all occurrences of handle_timeout"),
        dir.path(),
    );

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        answer.text.contains("stall.rs"),
        "search did not find the file: {}",
        answer.text
    );
}

#[test]
fn reads_one_function_body_rather_than_the_whole_file() {
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped(
            "show me the body of the handle_timeout function",
            "src/agent/stall.rs",
        ),
        dir.path(),
    );

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        answer.text.contains("handle_timeout"),
        "got: {}",
        answer.text
    );
    // The point of the capability: it did NOT return the neighbouring struct.
    assert!(
        !answer.text.contains("StallDetector"),
        "returned the whole file instead of one function: {}",
        answer.text
    );
}

// ---------------------------------------------------------------------------
// Injected seams: live model and web, with no network anywhere.
// ---------------------------------------------------------------------------

struct StubWeb(&'static str);
impl WebSearch for StubWeb {
    fn search(&self, _query: &str) -> Result<String, String> {
        Ok(self.0.to_string())
    }
}

#[test]
fn a_live_need_reaches_the_model_seam() {
    let dir = workspace();
    let backend = sc_model::MockBackend::new(["Because a trait keeps the runtime swappable."]);
    let gw = Gateway::new();
    let ctx = Ctx {
        workspace: dir.path(),
        model: Some(&backend),
        web: None,
        verify: None,
    };
    let answer = gw.ask_with(&Need::new("explain why this uses a trait"), &ctx);

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(answer.text.contains("swappable"), "got: {}", answer.text);
    assert_eq!(answer.trace.class, Some(sc_gateway::Class::Live));
}

#[test]
fn a_web_need_reaches_the_web_seam() {
    let dir = workspace();
    let web = StubWeb("serde 1.0.210 is current.");
    let gw = Gateway::new();
    let ctx = Ctx {
        workspace: dir.path(),
        model: None,
        web: Some(&web),
        verify: None,
    };
    let answer = gw.ask_with(&Need::new("check the latest serde version online"), &ctx);

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(answer.text.contains("1.0.210"));
    assert_eq!(answer.trace.class, Some(sc_gateway::Class::Web));
}

#[test]
fn the_model_router_breaks_a_tie_only_when_escalation_is_on() {
    let dir = workspace();
    let need = Need::new("the middle of the file");

    // Off (the default): the ambiguity stands as a refusal.
    let strict = Gateway::new();
    let refused = ask(&strict, &need, dir.path());
    assert!(refused.is_unknown(), "resolved a tie without escalation");

    // On: the router picks from the shortlist and the trace says so.
    // The mock must name something ON the shortlist — anything else is treated
    // as a hallucinated capability and refused, which the next test pins.
    let backend = sc_model::MockBackend::new(["file.read"]);
    let lenient = Gateway::new().with_escalation(true);
    let ctx = Ctx {
        workspace: dir.path(),
        model: Some(&backend),
        web: None,
        verify: None,
    };
    let routed = lenient.ask_with(&need, &ctx);
    assert_eq!(routed.trace.route.as_deref(), Some("file.read"));
    assert_eq!(routed.trace.reason, "llm-router");
}

#[test]
fn a_router_naming_something_off_the_shortlist_is_a_refusal() {
    // The single most dangerous router failure: a hallucinated capability name
    // must not be coerced into the nearest real one.
    let dir = workspace();
    let backend = sc_model::MockBackend::new(["file.delete_everything"]);
    let gw = Gateway::new().with_escalation(true);
    let ctx = Ctx {
        workspace: dir.path(),
        model: Some(&backend),
        web: None,
        verify: None,
    };
    let answer = gw.ask_with(&Need::new("the middle of the file"), &ctx);
    assert!(answer.is_unknown(), "accepted an invented capability name");
}

// ---------------------------------------------------------------------------
// The line window: the narrowing this crate exists to provide.
// ---------------------------------------------------------------------------

#[test]
fn a_line_range_narrows_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=200)
        .map(|n| {
            format!(
                "line {n}
"
            )
        })
        .collect();
    fs::write(dir.path().join("big.rs"), &body).unwrap();

    let gw = Gateway::new();
    let windowed = ask(&gw, &Need::new("read lines 40-60 of big.rs"), dir.path());
    assert!(!windowed.is_unknown(), "refused: {}", windowed.trace.reason);

    assert!(windowed.text.contains("line 40"), "window missed its start");
    assert!(windowed.text.contains("line 60"), "window missed its end");
    assert!(
        !windowed.text.contains("line 150"),
        "window returned the whole file: {} bytes",
        windowed.text.len()
    );
}

#[test]
fn a_read_with_no_range_is_not_silently_truncated() {
    // The counterpart risk: passing a window unconditionally would turn every
    // plain read into a partial one the caller never asked for.
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=120)
        .map(|n| {
            format!(
                "line {n}
"
            )
        })
        .collect();
    fs::write(dir.path().join("big.rs"), &body).unwrap();

    let gw = Gateway::new();
    let whole = ask(&gw, &Need::new("read big.rs"), dir.path());
    assert!(whole.text.contains("line 1"), "lost the top of the file");
    assert!(
        whole.text.contains("line 120"),
        "an unqualified read was truncated"
    );
}

#[test]
fn a_single_line_gets_context_around_it() {
    // One bare line almost never carries enough to act on.
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=200)
        .map(|n| {
            format!(
                "line {n}
"
            )
        })
        .collect();
    fs::write(dir.path().join("big.rs"), &body).unwrap();

    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("read line 88 of big.rs"), dir.path());
    assert!(answer.text.contains("line 88"), "missed the named line");
    assert!(answer.text.contains("line 85"), "no context before");
    assert!(answer.text.contains("line 91"), "no context after");
    assert!(!answer.text.contains("line 150"), "window too wide");
}

// ---------------------------------------------------------------------------
// The capabilities added after the first pass, which reached only 5 of 18 tools.
// ---------------------------------------------------------------------------

#[test]
fn cargo_info_describes_a_real_workspace() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("crates/demo-core/src")).unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]
members = [\"crates/demo-core\"]
",
    )
    .unwrap();
    fs::write(
        dir.path().join("crates/demo-core/Cargo.toml"),
        "[package]
name = \"demo-core\"
version = \"0.1.0\"
",
    )
    .unwrap();

    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::new("what crates are in this workspace"),
        dir.path(),
    );
    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert_eq!(answer.trace.route.as_deref(), Some("cargo.info"));
    assert!(
        answer.text.contains("demo-core"),
        "did not name the crate: {}",
        answer.text
    );
}

/// A folded-stack profile: one line per unique stack, `a;b;c <samples>`.
/// The shape `profile_hotspots` reads, written the way the Profiler panel does.
fn write_profile(root: &Path) {
    let folded = "main;parse;lex 4200
main;parse;build_ast 1800
main;typecheck;unify 900
main;typecheck;subst 400
main;codegen;emit 250
main;io;read_file 90
main;io;write_file 60
";
    fs::create_dir_all(root.join("target")).unwrap();
    fs::write(root.join("target/sc-profile.folded"), folded).unwrap();
}

#[test]
fn hotspots_reads_a_real_folded_profile() {
    // The tool added to the registry after the first pass. It parses through
    // sc-flame, so this exercises the real parser, not a gateway stand-in.
    let dir = workspace();
    write_profile(dir.path());

    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("what are the hotspots", "target/sc-profile.folded"),
        dir.path(),
    );

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert_eq!(answer.trace.route.as_deref(), Some("perf.hotspots"));
    // The hottest frame by self time must lead.
    assert!(
        answer.text.contains("lex"),
        "did not report the hottest frame: {}",
        answer.text
    );
}

#[test]
fn hotspots_honours_a_stated_row_count() {
    // "top 3" is a real narrowing — the whole premise of the crate is that the
    // model gets the minimum it asked for, not a default-sized dump.
    let dir = workspace();
    write_profile(dir.path());

    let gw = Gateway::new();
    let narrow = ask(
        &gw,
        &Need::scoped("show the top 3 hotspots", "target/sc-profile.folded"),
        dir.path(),
    );
    assert!(!narrow.is_unknown(), "refused: {}", narrow.trace.reason);

    // Three ranked rows, and not a fourth.
    assert!(narrow.text.contains("  3. "), "fewer than 3 rows returned");
    assert!(
        !narrow.text.contains("  4. "),
        "the stated limit was ignored: {}",
        narrow.text
    );
}

#[test]
fn a_need_with_no_count_takes_the_tools_own_default() {
    // The counterpart risk: inventing a limit the caller never stated would be
    // the gateway silently truncating, which is the failure it exists to avoid.
    let dir = workspace();
    write_profile(dir.path());

    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("what are the hotspots", "target/sc-profile.folded"),
        dir.path(),
    );
    // This profile has 7 leaf frames, under the tool's default of 15, so all
    // of them survive when the gateway adds no limit of its own.
    assert!(
        answer.text.contains("write_file"),
        "the coldest frame was dropped without being asked for: {}",
        answer.text
    );
}

#[test]
fn a_missing_profile_reports_the_tools_own_guidance() {
    // The common case on a repo nobody has profiled. The tool's message names
    // the expected format and where the panel writes it — more useful than a
    // gateway-authored error, so the gateway must not replace it.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("what are the hotspots", "target/sc-profile.folded"),
        dir.path(),
    );

    assert!(
        !answer.is_unknown(),
        "reported a missing file as a misroute"
    );
    assert!(
        answer.text.contains("folded") || answer.text.contains("Could not read"),
        "lost the tool's guidance: {}",
        answer.text
    );
}

#[test]
fn verification_refuses_cleanly_with_no_command_configured() {
    // The gateway never invents a test command — a wrong guess would run
    // something arbitrary and report its output as the suite result.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("run the tests"), dir.path());
    assert!(answer.is_unknown(), "ran a suite with nothing configured");
    assert!(
        answer.trace.reason.contains("unavailable"),
        "unexpected reason: {}",
        answer.trace.reason
    );
}

#[test]
fn verification_runs_the_configured_command_and_reports_failures() {
    // The capability that finally gives the simplifier real output to work on.
    // A trivially failing command stands in for a suite, so the test is fast
    // and needs no cargo project.
    let dir = workspace();
    let sandbox = sc_verify::Sandbox::Host;
    let verify = sc_gateway::Verify {
        sandbox: &sandbox,
        command: "exit 1",
    };
    let gw = Gateway::new();
    let ctx = Ctx {
        workspace: dir.path(),
        model: None,
        web: None,
        verify: Some(&verify),
    };
    let answer = gw.ask_with(&Need::new("run the tests"), &ctx);

    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert_eq!(answer.trace.route.as_deref(), Some("verify.run"));
    assert_eq!(answer.trace.class, Some(sc_gateway::Class::Verify));
    assert!(
        answer.text.contains("failed") || answer.text.contains("non-zero"),
        "a failing suite did not report as failing: {}",
        answer.text
    );
}

#[test]
fn verification_reports_a_green_suite_as_green() {
    let dir = workspace();
    let sandbox = sc_verify::Sandbox::Host;
    let verify = sc_gateway::Verify {
        sandbox: &sandbox,
        command: "exit 0",
    };
    let gw = Gateway::new();
    let ctx = Ctx {
        workspace: dir.path(),
        model: None,
        web: None,
        verify: Some(&verify),
    };
    let answer = gw.ask_with(&Need::new("are the tests passing"), &ctx);
    assert!(!answer.is_unknown(), "refused: {}", answer.trace.reason);
    assert!(
        answer.text.contains("passed") || answer.text.contains("exited 0"),
        "a green suite did not report as green: {}",
        answer.text
    );
}

// ---------------------------------------------------------------------------
// Failure reporting: a capability that ran and found nothing must not look
// like a classifier refusal. They have different fixes.
// ---------------------------------------------------------------------------

#[test]
fn a_missing_file_is_an_executor_failure_not_a_refusal() {
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("read src/does_not_exist.rs"), dir.path());

    assert!(
        !answer.is_unknown(),
        "a missing file was reported as a routing refusal"
    );
    assert_eq!(answer.trace.route.as_deref(), Some("file.read"));
}

#[test]
fn the_gateway_cannot_escape_the_workspace() {
    // The gateway routes; it does not get to widen the sandbox. This inherits
    // the registry path checks by construction, and this test pins that.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(
        &gw,
        &Need::scoped("read the file", "../../../etc/passwd"),
        dir.path(),
    );
    assert!(
        !answer.text.contains("root:"),
        "escaped the workspace: {}",
        answer.text
    );
}

// ---------------------------------------------------------------------------
// The measurement. This is the number that decides whether the whole idea is
// worth wiring in.
// ---------------------------------------------------------------------------

#[test]
fn every_answer_carries_a_complete_trace() {
    // Without this, a misroute, a dry capability and an over-eager simplifier
    // are indistinguishable after the fact.
    let dir = workspace();
    let gw = Gateway::new();
    for need in [
        Need::new("read src/lib.rs"),
        Need::scoped("list the files", "src"),
        Need::new("search for run_demo"),
    ] {
        let answer = ask(&gw, &need, dir.path());
        assert!(
            answer.trace.route.is_some(),
            "no route recorded for {:?}",
            need.text
        );
        assert!(
            !answer.trace.reason.is_empty(),
            "no reason recorded for {:?}",
            need.text
        );
        assert!(answer.trace.out_bytes > 0);
    }
}

#[test]
fn the_model_facing_surface_is_one_argument_wide() {
    // The premise of the whole crate: whatever the model sees is one string in
    // and one string out. If a future change leaks structure into the model's
    // view, this is where it shows up.
    let dir = workspace();
    let gw = Gateway::new();
    let answer = ask(&gw, &Need::new("read src/lib.rs"), dir.path());
    let _: &str = &answer.text;
    assert!(
        !answer.text.contains("\"tool\""),
        "leaked tool-call structure to the model: {}",
        answer.text
    );
}

#[test]
fn report_reduction_across_the_capability_surface() {
    // Not an assertion on a magic number — a printed baseline. Run with
    // `cargo test -p sc-gateway -- --nocapture` to see the table before and
    // after any change to the simplifier.
    let dir = workspace();
    let gw = Gateway::new();
    let needs = [
        Need::new("read src/lib.rs"),
        Need::scoped("list the files", "src"),
        Need::new("search for handle_timeout"),
    ];

    println!("\n{:<38} {:>8} {:>8} {:>7}", "need", "raw", "out", "kept");
    let mut total_raw = 0usize;
    let mut total_out = 0usize;
    for need in &needs {
        let answer = ask(&gw, need, dir.path());
        total_raw += answer.trace.raw_bytes;
        total_out += answer.trace.out_bytes;
        println!(
            "{:<38} {:>8} {:>8} {:>6}%",
            need.text,
            answer.trace.raw_bytes,
            answer.trace.out_bytes,
            answer.trace.retained_percent()
        );
    }
    println!("{:<38} {total_raw:>8} {total_out:>8}", "TOTAL");

    // The only hard guarantee: reduction never invents bytes.
    assert!(total_out <= total_raw, "simplification grew the output");
}

#[test]
fn report_reduction_on_real_verification_output() {
    // The measurement that matters. The synthetic-workspace baseline above
    // shows 100% retained because tiny fixtures carry no waste — the
    // extractors only fire on shapes that DO. Real cargo output is that shape.
    //
    // Uses a canned-but-real cargo failure rather than compiling a crate, so
    // the number is reproducible and the test stays fast.
    const REAL: &str = include_str!("fixtures/cargo_failure.txt");

    let mut trace = sc_gateway::Trace::default();
    let out = sc_gateway::simplify(REAL, sc_gateway::Level::Extract, &mut trace, None);

    println!(
        "
real cargo failure: {} -> {} bytes ({}% retained)",
        trace.raw_bytes,
        trace.out_bytes,
        trace.retained_percent()
    );
    println!("dropped: {:?}", trace.dropped);

    // The signal survives...
    assert!(out.contains("E0308"), "lost the error code");
    assert!(out.contains("types.rs"), "lost the error location");
    assert!(
        out.contains("expected `usize`"),
        "lost the error detail — the one thing needed to fix it"
    );
    // ...and the noise does not.
    assert!(
        trace.retained_percent() < 70,
        "real cargo output barely shrank: {}% retained",
        trace.retained_percent()
    );
}
