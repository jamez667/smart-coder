//! M1 exit-criterion test: **≥95% valid tool calls**, and malformed calls are
//! *never* executed — only recovered (spec 07, spec 03).
//!
//! These are deterministic, model-free checks of the reliability mechanism: a
//! corpus of realistic small-model outputs run through the real extractor, and a
//! loop-level check that a bad call is fed back for repair rather than acted on.
//! The live, real-model valid-rate is measured separately via `dc-eval`.

use dc_core::{run_agent, AgentConfig, ParseRepair, ToolCallStrategy};
use dc_model::MockBackend;
use dc_tools::default_registry;

/// A corpus of outputs a small model realistically emits when asked for a tool
/// call. The harness must extract a valid call from the well-formed-but-noisy
/// ones (prose wrappers, trailing chatter) — those are *not* failures.
const NOISY_BUT_VALID: &[&str] = &[
    r#"{"tool":"read_file","path":"src/main.rs"}"#,
    "Sure! Here is the call:\n{\"tool\":\"list_dir\",\"path\":\".\"}",
    "{\"tool\":\"search_code\",\"query\":\"fn main\"}\nThat should find it.",
    "I'll write the file.\n{\"tool\":\"write_file\",\"path\":\"a.txt\",\"content\":\"hi { } there\"}",
    "```json\n{\"tool\":\"finish\"}\n```",
    "Let me read it: {\"tool\":\"read_file\",\"path\":\"Cargo.toml\"}",
    r#"{"tool":"read_file","path":"lib.rs"} done"#,
];

/// Genuinely malformed outputs: these *should* be flagged and repaired.
const MALFORMED: &[&str] = &[
    "I think I should read the file now.", // no JSON
    r#"{"tool":"read_file"}"#,             // missing required path
    r#"{"tool":"teleport","path":"x"}"#,   // unknown tool
];

#[test]
fn parse_repair_recovers_at_least_95_percent_of_noisy_valid_output() {
    let reg = default_registry();
    let strat = ParseRepair;
    let recovered = NOISY_BUT_VALID
        .iter()
        .filter(|raw| strat.extract(raw, &reg).is_ok())
        .count();
    let rate = recovered as f64 / NOISY_BUT_VALID.len() as f64;
    assert!(
        rate >= 0.95,
        "valid-call recovery {rate:.2} below the M1 0.95 target ({recovered}/{})",
        NOISY_BUT_VALID.len()
    );
}

#[test]
fn malformed_output_is_always_flagged_never_silently_accepted() {
    let reg = default_registry();
    for raw in MALFORMED {
        assert!(
            ParseRepair.extract(raw, &reg).is_err(),
            "malformed output was wrongly accepted: {raw:?}"
        );
    }
}

#[test]
fn a_malformed_call_is_repaired_not_executed_in_the_loop() {
    // The model emits a write to a file, but malformed (missing `content`), then
    // a correct finish. The malformed write must NOT create the file.
    let ws = std::env::temp_dir().join(format!(
        "dc-core-validity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&ws).unwrap();

    let backend = MockBackend::new([
        r#"{"tool":"write_file","path":"should_not_exist.txt"}"#.to_string(), // no content
        r#"{"tool":"finish"}"#.to_string(),
    ]);
    let report = run_agent(&backend, "go", &ws, &AgentConfig::default()).unwrap();

    assert!(report.finished);
    assert_eq!(
        report.metrics.invalid, 1,
        "the bad write should count invalid"
    );
    assert_eq!(report.metrics.valid, 1, "only finish is valid");
    assert!(
        !ws.join("should_not_exist.txt").exists(),
        "a malformed call must never touch the workspace"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn end_to_end_run_clears_the_95_percent_bar() {
    // A full run whose turns mirror real usage: many valid calls, one stray
    // malformed line the model self-corrects from. The aggregate must clear 0.95.
    let ws = std::env::temp_dir().join(format!(
        "dc-core-validity-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();

    // 19 valid reads + 1 malformed + finish would be 21 turns; cap steps high.
    let mut script: Vec<String> = (0..19)
        .map(|_| r#"{"tool":"read_file","path":"a.txt"}"#.to_string())
        .collect();
    script.push("oops, thinking out loud".to_string()); // 1 malformed
    script.push(r#"{"tool":"finish"}"#.to_string());

    // MockBackend advertises ToolCalling::None, so run_agent selects ParseRepair.
    // This test measures the valid-call rate over a deliberately repetitive script,
    // so raise the stall thresholds above the turn count (loop detection is its own
    // test, in tdd_loop / the recovery integration test).
    let backend = MockBackend::new(script);
    let cfg = AgentConfig {
        max_steps: 30,
        repeat_limit: 100,
        no_progress_limit: 100,
        ..Default::default()
    };
    let report = run_agent(&backend, "read repeatedly", &ws, &cfg).unwrap();

    assert!(report.finished);
    assert_eq!(report.metrics.invalid, 1);
    assert!(
        report.metrics.valid_rate() >= 0.95,
        "aggregate valid rate {:.3} < 0.95 ({} valid / {} total)",
        report.metrics.valid_rate(),
        report.metrics.valid,
        report.metrics.total()
    );

    let _ = std::fs::remove_dir_all(&ws);
}
