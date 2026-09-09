//! Never run the verify command when the answer cannot have changed.
//!
//! MEASURED WASTE. On a real 15-turn refactor (117s end to end) the verify command ran FIVE
//! times at ~6s each -- roughly 30s, a quarter of the run: the run-start baseline, an
//! auto-verify after an edit, an auto-verify after another edit, a `run_verification` the
//! model asked for itself, and the finish gate. Several were redundant -- nothing had written
//! to the workspace between them. `cargo check` on this workspace is 6s; a project with a real
//! test suite pays minutes for the same nothing.
//!
//! These tests COUNT the invocations rather than inferring them. The verify command appends a
//! line to a file in the workspace before running the real check, so the line count IS the
//! number of times the command ran -- there is no way for a change in the loop to fake it.
//!
//! The safety cases matter more than the savings: after a real change the suite MUST re-run,
//! a red suite MUST still refuse `finish`, and a skip must never report a red suite as green.

use std::cell::RefCell;
use std::path::Path;

use sc_core::{run_agent_with, AgentConfig, ParseRepair};
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use sc_tools::{default_registry, PermissionPolicy};

/// A backend that replays a fixed script of tool-call JSON, one per turn, then repeats a
/// harmless read so the run ends at the step budget rather than volunteering a `finish` that
/// would end it for a different reason (see `Looping` in tdd_loop.rs).
struct Looping(RefCell<Vec<String>>);
impl Looping {
    fn new(turns: Vec<&str>) -> Self {
        Looping(RefCell::new(turns.into_iter().map(String::from).collect()))
    }
}
impl ModelBackend for Looping {
    fn name(&self) -> &str {
        "looping"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        let mut s = self.0.borrow_mut();
        let content = if s.len() > 1 {
            s.remove(0)
        } else {
            s.first().cloned().unwrap_or_default()
        };
        Ok(GenerateResponse::new(content))
    }
}

fn temp_repo(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-fresh-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The counting verify command: one line appended per invocation, then the real check.
///
/// `verify_count.txt` lives in the workspace but is never touched by any tool the model can
/// call, so it cannot be confused with a model edit.
const COUNTING_VERIFY: &str = "sh -c 'echo x >> verify_count.txt; sh test.sh'";

fn verify_runs(ws: &Path) -> usize {
    std::fs::read_to_string(ws.join("verify_count.txt"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// A repo whose suite is GREEN before the agent starts -- the refactor shape the waste was
/// measured on. `test.sh` is frozen so nothing can weaken it.
fn green_repo(tag: &str) -> std::path::PathBuf {
    let ws = temp_repo(tag);
    std::fs::write(
        ws.join("impl.sh"),
        "is_even() { [ $(( $1 % 2 )) -eq 0 ]; }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("test.sh"),
        ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
    )
    .unwrap();
    ws
}

/// A repo whose suite is RED: `impl.sh` is a wrong stub.
fn red_repo(tag: &str) -> std::path::PathBuf {
    let ws = temp_repo(tag);
    std::fs::write(ws.join("impl.sh"), "is_even() { return 1; }\n").unwrap();
    std::fs::write(
        ws.join("test.sh"),
        ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
    )
    .unwrap();
    ws
}

fn counting_config() -> AgentConfig {
    AgentConfig {
        verify_command: Some(COUNTING_VERIFY.to_string()),
        permission: PermissionPolicy::with_frozen(vec!["test.sh".to_string()]),
        ..Default::default()
    }
}

fn run(backend: &dyn ModelBackend, ws: &Path, cfg: &AgentConfig) -> sc_core::AgentReport {
    let registry = default_registry();
    run_agent_with(
        backend,
        &registry,
        &ParseRepair,
        "Refactor is_even without changing its behaviour.",
        ws,
        cfg,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// The savings.
// ---------------------------------------------------------------------------

#[test]
fn an_edit_then_finish_verifies_once_for_that_edit_not_twice() {
    // The headline case, and invocations 3 and 5 of the measured run. An edit lands, the
    // harness auto-verifies it green, and the model immediately calls `finish`. Nothing wrote
    // to the workspace in between, so the finish gate has nothing to learn by running the
    // suite again -- it honours the recorded green.
    //
    // Counted: 1 baseline + 1 auto-verify = 2. Before this change it was 3.
    let ws = green_repo("edit-finish");
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo hi\n"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 6,
        ..counting_config()
    };

    let report = run(&backend, &ws, &cfg);

    assert!(
        report.finished,
        "an explicit finish on a green suite must still be honoured; stopped {:?}",
        report.stop_reason
    );
    assert_eq!(report.verified, Some(true), "and it must report verified");
    assert_eq!(
        verify_runs(&ws),
        2,
        "expected the baseline + ONE auto-verify; the finish gate must not re-run it"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_second_run_verification_with_no_change_between_replays_the_first() {
    // Invocation 4 of the measured run: the model re-checking a suite nothing had touched.
    // Both calls get the recorded answer with a note saying why, and the command is never
    // spawned for either.
    //
    // Counted: 1 baseline + 1 edit auto-verify = 2. The edit's auto-verify already answered
    // for this workspace, so BOTH model-invoked calls are served from the record -- the loop
    // saves two invocations here, not one.
    let ws = green_repo("double-verify");
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo hi\n"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 6,
        ..counting_config()
    };

    let report = run(&backend, &ws, &cfg);

    // The run must not have ended early for some other reason, or the second
    // `run_verification` never happened and the count would be trivially low.
    assert!(
        report.steps >= 3,
        "the script must have reached the second run_verification, took {} steps",
        report.steps
    );
    assert_eq!(
        verify_runs(&ws),
        2,
        "neither run_verification had anything to learn; both must be served from the record"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_replayed_observation_says_why_it_was_not_re_run() {
    // The replay is the text the MODEL reads, so it must be well-formed prose AND carry the
    // previous result -- a model handed a bare note with no result, or a mangled sentence,
    // re-asks, which is the habit this whole change exists to break.
    //
    // Counting invocations cannot catch this: a mangled note runs the command exactly as often
    // as a clean one. Asserting on the real observation does, and it caught a real defect --
    // an eaten line-continuation left runs of padding spaces mid-sentence.
    let ws = green_repo("replay-text");
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo hi\n"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 5,
        ..counting_config()
    };

    // Capture the real event stream so the assertion reads the observation the loop actually
    // produced, not one the test reconstructed.
    let log = sc_core::runlog::RunLogSink::new();
    let registry = default_registry();
    let report = sc_core::run_agent_observed(
        &backend,
        None,
        &registry,
        &ParseRepair,
        "Refactor is_even without changing its behaviour.",
        &ws,
        &cfg,
        &log,
    )
    .unwrap();
    assert!(report.steps >= 2, "the script must have reached the verify");

    let replayed = log
        .lock()
        .events()
        .iter()
        .find_map(|e| match e {
            sc_core::event::AgentEvent::ToolResult { full, .. }
                if full.contains("nothing has changed in the workspace") =>
            {
                Some(full.clone())
            }
            _ => None,
        })
        .expect("the second verification should have been served from the record");

    assert!(
        replayed.contains(
            "nothing has changed in the workspace since this ran, so the harness \
                           did not run it again"
        ),
        "the note must read as one properly spaced sentence, got:\n{replayed}"
    );
    assert!(
        replayed
            .lines()
            .any(|l| l.contains("passed") || l.contains("✓")),
        "the replay must carry the PREVIOUS RESULT, not just the note:\n{replayed}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// The safety. A skip that outlives a change is the bug this guards against.
// ---------------------------------------------------------------------------

#[test]
fn a_change_after_a_green_makes_the_next_verification_run_for_real() {
    // The whole point of the flag, isolated. Verify green, change the workspace, verify
    // again: the second call MUST spawn the command, because the answer can have moved. A
    // stale green here is exactly the class of bug the optimisation must not introduce.
    //
    // The change is made with `run_command` deliberately. An `edit_file`/`create_file` turn
    // triggers the harness's own auto-verify, which would satisfy the assertion for the wrong
    // reason -- it would prove the auto-verify ran, not that the flag reset. A `run_command`
    // has no auto-verify, so the only thing that can produce a third invocation is the model's
    // `run_verification` finding the recorded green invalidated.
    //
    // Counted: 1 baseline + 1 model-invoked (green recorded) + 1 model-invoked AFTER the
    // change = 3. Take the `run_command` dirtying away and this is 2.
    let ws = green_repo("change-invalidates");
    let backend = Looping::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"run_command","command":"echo '# touched' >> impl.sh"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 6,
        permission: PermissionPolicy {
            allow_shell: true,
            ..PermissionPolicy::with_frozen(vec!["test.sh".to_string()])
        },
        ..counting_config()
    };

    let report = run(&backend, &ws, &cfg);

    assert!(
        report.steps >= 3,
        "the script must have reached the second run_verification, took {} steps",
        report.steps
    );
    assert_eq!(
        verify_runs(&ws),
        3,
        "the change invalidated the recorded green; the verification after it MUST have run"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_run_command_invalidates_the_green_even_though_no_tool_path_changed() {
    // A shell command can write anything -- `sed -i`, a codegen step, a build -- without ever
    // touching a path the journal snapshots, so the loop's `changed` flag stays false. It must
    // still count as a write, or a command that breaks the build is followed by a `finish`
    // honoured against a green recorded BEFORE it.
    //
    // Here the command genuinely does break the build, and the finish gate must catch it.
    let ws = green_repo("run-command-dirties");
    let backend = Looping::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"run_command","command":"echo 'is_even() { return 1; }' > impl.sh"}"#,
        r#"{"tool":"finish"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 6,
        permission: PermissionPolicy {
            allow_shell: true,
            ..PermissionPolicy::with_frozen(vec!["test.sh".to_string()])
        },
        ..counting_config()
    };

    let report = run(&backend, &ws, &cfg);

    // Sanity: the command really did break the suite, otherwise this proves nothing.
    let impl_after = std::fs::read_to_string(ws.join("impl.sh")).unwrap();
    assert!(
        impl_after.contains("return 1"),
        "the run_command must have rewritten impl.sh, got: {impl_after}"
    );
    assert!(
        !report.finished,
        "a finish after a build-breaking run_command must NOT be honoured on a stale green"
    );
    assert_ne!(
        report.verified,
        Some(true),
        "a skip must never report a red suite as green"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_red_suite_still_refuses_finish_and_is_never_reported_green() {
    // The other direction: a RED result is never reused. The model must be able to fix it and
    // learn whether the fix landed, and `finish` must stay refused while it has not.
    let ws = red_repo("red-refuses");
    let backend = Looping::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"finish"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 4,
        ..counting_config()
    };

    let report = run(&backend, &ws, &cfg);

    assert!(!report.finished, "a red suite must not finish");
    assert_eq!(
        report.verified,
        Some(false),
        "and must never be reported green"
    );
    assert_eq!(
        report.started_green,
        Some(false),
        "the baseline must have recorded red"
    );
    // Every one of those refusals re-ran the command: nothing is cached on the red path.
    assert!(
        verify_runs(&ws) >= 3,
        "a red result must never be replayed, saw {} runs",
        verify_runs(&ws)
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_run_start_baseline_is_never_skipped() {
    // The baseline establishes the shape of the run and there is nothing recorded before it to
    // reuse -- it must run even on a run that does nothing at all.
    let ws = green_repo("baseline-always");
    let backend = Looping::new(vec![r#"{"tool":"read_file","path":"impl.sh"}"#]);
    let cfg = AgentConfig {
        max_steps: 2,
        ..counting_config()
    };

    let report = run(&backend, &ws, &cfg);

    assert_eq!(report.started_green, Some(true));
    assert_eq!(
        verify_runs(&ws),
        1,
        "the baseline runs exactly once, and a read-only run adds nothing"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
