//! M3 exit-criterion test (spec 07 / spec 11): given a failing unit test, the
//! agent drives it red→green on a sample repo **without breaking the suite or
//! weakening the test**.
//!
//! We script the model (no live LLM needed) to exercise the full M3 path:
//! verify (red) → anchored edit → verify (green) → finish, gated on the
//! whole-suite verification. Separately we prove the frozen contract test can't
//! be cheated and that `finish` is refused while the suite is red.

use std::cell::RefCell;
use std::path::Path;

use sc_core::{run_agent_with, AgentConfig, ParseRepair};
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use sc_tools::{default_registry, PermissionPolicy};

/// A backend that replays a fixed script of tool-call JSON, one per turn.
struct Scripted(RefCell<Vec<String>>);
impl Scripted {
    fn new(turns: Vec<&str>) -> Self {
        Scripted(RefCell::new(turns.into_iter().map(String::from).collect()))
    }
}
impl ModelBackend for Scripted {
    fn name(&self) -> &str {
        "scripted"
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
        let content = if s.is_empty() {
            r#"{"tool":"finish"}"#.to_string()
        } else {
            s.remove(0)
        };
        Ok(GenerateResponse::new(content))
    }
}

/// Like [`Scripted`], but when the script runs out it REPEATS the last turn instead of
/// falling back to `finish`.
///
/// The fallback is fine for a TDD test -- the run ends on the green anyway -- but it makes
/// the baseline tests below unfalsifiable: they need to prove the loop does NOT end itself
/// on a green suite, and a backend that volunteers `finish` the moment the script is
/// exhausted ends the run for a completely different reason. This one keeps calling a
/// harmless read until the step budget runs out, which is what "the run kept going" means.
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
        "sc-core-tdd-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A red sample repo: `impl.sh` is a wrong stub; `test.sh` is the contract test
/// that fails until the impl is fixed.
fn red_repo() -> std::path::PathBuf {
    let ws = temp_repo("repo");
    std::fs::write(ws.join("impl.sh"), "is_even() { return 1; }\n").unwrap();
    std::fs::write(
        ws.join("test.sh"),
        ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
    )
    .unwrap();
    ws
}

fn config_with_verify() -> AgentConfig {
    AgentConfig {
        verify_command: Some("sh test.sh".to_string()),
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
        "Make is_even report even numbers correctly.",
        ws,
        cfg,
    )
    .unwrap()
}

#[test]
fn agent_drives_a_failing_test_red_to_green() {
    let ws = red_repo();
    // verify (red) -> fix impl with an anchored edit -> verify (green) -> finish.
    let backend = Scripted::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"[ $(( $1 % 2 )) -eq 0 ];"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"finish"}"#,
    ]);

    let report = run(&backend, &ws, &config_with_verify());

    assert!(report.finished, "should finish");
    assert_eq!(report.verified, Some(true), "whole suite must be green");
    // The contract test is byte-for-byte unchanged (not weakened).
    let test_after = std::fs::read_to_string(ws.join("test.sh")).unwrap();
    assert!(
        test_after.contains("is_even 4 || exit 1"),
        "test was altered"
    );
    // The journal recorded the impl edit.
    assert!(
        report.change_summary.contains("impl.sh"),
        "{}",
        report.change_summary
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn auto_finishes_when_verification_passes_without_a_finish_call() {
    // A small model often fixes the code and runs the tests green, then keeps
    // going (or stalls) instead of calling `finish`. The harness should honor the
    // win: a green run_verification ends the run as Finished on its own.
    let ws = red_repo();
    let backend = Scripted::new(vec![
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"[ $(( $1 % 2 )) -eq 0 ];"}"#,
        r#"{"tool":"run_verification"}"#,
        // No finish — and the script would otherwise loop on read_file forever.
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);

    let report = run(&backend, &ws, &config_with_verify());

    assert!(report.finished, "green verification should auto-finish");
    assert_eq!(report.verified, Some(true));
    // It stopped right at the green verification, not at the step budget.
    assert!(
        report.steps <= 2,
        "should finish promptly, took {}",
        report.steps
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn finish_is_refused_while_the_suite_is_red() {
    let ws = red_repo();
    // The model tries to finish without fixing anything. The gate refuses, so the
    // run hits the step budget rather than a bogus green finish.
    let backend = Scripted::new(vec![r#"{"tool":"finish"}"#]);
    let cfg = AgentConfig {
        max_steps: 3,
        ..config_with_verify()
    };
    let report = run(&backend, &ws, &cfg);
    assert!(!report.finished, "must not finish on a red suite");
    assert_eq!(report.verified, Some(false));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cheating_by_editing_the_frozen_test_is_denied() {
    let ws = red_repo();
    // The model tries to neuter the contract test, then finish. The permission
    // gate denies the edit; the test file is untouched; finish stays refused.
    let backend = Scripted::new(vec![
        r#"{"tool":"edit_file","path":"test.sh","old_str":"is_even 4 || exit 1","new_str":"true"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 4,
        ..config_with_verify()
    };
    let report = run(&backend, &ws, &cfg);

    assert!(!report.finished, "cheating must not yield a finish");
    // The contract test is intact.
    let test_after = std::fs::read_to_string(ws.join("test.sh")).unwrap();
    assert!(
        test_after.contains("is_even 4 || exit 1"),
        "frozen test was edited!"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// The BASELINE: what the suite was doing before the agent touched anything.
//
// Every auto-finish in the loop assumed the TDD premise above -- the suite starts
// RED, so going green is proof of progress. On a REFACTOR the suite is green before
// the first turn, and green then proves nothing.
//
// Found on mini-miner-2, a real 8,190-line file. Task: extract five multiplayer
// methods from app.rs into a new app/net.rs, add `mod net;`, delete the moved
// bodies, `cargo check -p miner` must pass. The model created app/net.rs with
// mangled empty bodies, never added the module declaration, never deleted the
// originals, and called run_verification. cargo check passed -- an UNREFERENCED new
// file changes nothing -- and the loop auto-finished `finished: true, verified:
// Some(true)` after 8 steps, with a third of the job done and a generated file that
// would not have compiled if it had been wired in.
// ---------------------------------------------------------------------------

/// A GREEN sample repo: the "suite" already passes, exactly as it does at the start of
/// any refactor. Nothing here is red for the agent to fix.
fn green_repo() -> std::path::PathBuf {
    let ws = temp_repo("green");
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

#[test]
fn a_green_verification_on_a_green_at_start_run_is_not_a_finish() {
    // THE REGRESSION. The verify command already passes. The model writes ONE
    // unrelated new file -- the harmless, unreferenced kind that cannot change the
    // build's result either way -- and then asks for the tests. Green comes back,
    // because green is what the workspace was handed over as.
    //
    // That must not be reported as a verified success: nothing was proven.
    let ws = green_repo();
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo nothing references this\n"}"#,
        r#"{"tool":"run_verification"}"#,
        // If the loop were still auto-finishing, it would never reach these turns.
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 6,
        ..config_with_verify()
    };

    let report = run(&backend, &ws, &cfg);

    assert_eq!(
        report.started_green,
        Some(true),
        "the baseline must be recorded, and this suite starts green"
    );
    assert!(
        !report.finished,
        "a green verification on a green-at-start run is not proof the task is done; \
         stop_reason was {:?}",
        report.stop_reason
    );
    assert_ne!(
        report.stop_reason,
        sc_core::StopReason::Finished,
        "the run must not report Finished off a baseline-green verification"
    );
    // The unreferenced file really was written -- the point is that writing it, and
    // the build still passing, is not a completed task.
    assert!(ws.join("net.sh").exists(), "the scripted write should land");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_edit_on_a_green_at_start_run_does_not_auto_finish_either() {
    // The other auto-finish site: the harness runs the suite itself the moment an
    // edit lands. On a refactor that fires on the FIRST edit, with the whole job
    // still ahead of it -- this is the branch that actually shipped the mini-miner-2
    // false success, since the model never had to call run_verification at all.
    let ws = green_repo();
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo unreferenced\n"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 4,
        ..config_with_verify()
    };

    let report = run(&backend, &ws, &cfg);

    assert_eq!(report.started_green, Some(true));
    assert!(
        !report.finished,
        "the post-edit auto-verify must not finish a run that started green"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn finish_is_still_honoured_when_the_run_started_green() {
    // Nothing else is left to gate on, so the model's own `finish` IS the signal:
    // `gate_finish` runs the suite, it is green, and the run ends as Finished. Take
    // this away and a refactor could never complete at all.
    let ws = green_repo();
    let backend = Scripted::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo unreferenced\n"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 5,
        ..config_with_verify()
    };

    let report = run(&backend, &ws, &cfg);

    assert!(
        report.finished,
        "an explicit finish must still be honoured; stopped {:?}",
        report.stop_reason
    );
    assert_eq!(report.verified, Some(true));
    assert_eq!(report.started_green, Some(true));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_tdd_path_is_unchanged_and_records_a_red_baseline() {
    // The premise the auto-finish was built on is still intact: a suite that starts
    // RED and goes green is the agent's doing, so the win is still honoured without
    // an explicit `finish`.
    let ws = red_repo();
    let backend = Scripted::new(vec![
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"[ $(( $1 % 2 )) -eq 0 ];"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);

    let report = run(&backend, &ws, &config_with_verify());

    assert_eq!(
        report.started_green,
        Some(false),
        "a red-at-start run must record a red baseline"
    );
    assert!(
        report.finished,
        "green verification should still auto-finish"
    );
    assert_eq!(report.verified, Some(true));
    assert!(
        report.steps <= 2,
        "should still finish promptly, took {}",
        report.steps
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn no_verify_command_means_no_baseline_was_measured() {
    // The guard: with nothing configured there is nothing to run, and the extra
    // verification the baseline costs must not be spent. `None` says "not measured",
    // which is not the same as "started red".
    let ws = green_repo();
    let backend = Scripted::new(vec![r#"{"tool":"finish"}"#]);
    let cfg = AgentConfig {
        max_steps: 3,
        ..Default::default()
    };
    assert!(cfg.verify_command.is_none());

    let report = run(&backend, &ws, &cfg);

    assert_eq!(report.started_green, None);
    assert_eq!(report.verified, None);
    assert!(report.finished);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_baseline_is_emitted_once_on_the_event_stream() {
    // A transcript that cannot see the baseline cannot tell a refactor-shaped run
    // from a TDD-shaped one -- and the cost guard is only real if it is one run, so
    // assert the count, not just the presence.
    use std::sync::Mutex;
    let ws = green_repo();
    let backend = Scripted::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo unreferenced\n"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 5,
        ..config_with_verify()
    };

    let seen: Mutex<Vec<sc_core::AgentEvent>> = Mutex::new(Vec::new());
    let sink = sc_core::FnSink(|e: &sc_core::AgentEvent| seen.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    sc_core::run_agent_observed(
        &backend,
        None,
        &registry,
        &ParseRepair,
        "Extract the net helpers into their own file.",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    let events = seen.lock().unwrap();
    let baselines: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            sc_core::AgentEvent::BaselineVerification { green, .. } => Some(*green),
            _ => None,
        })
        .collect();
    assert_eq!(
        baselines,
        vec![true],
        "exactly one baseline, and it says the suite started green"
    );
    // It is taken BEFORE the first turn, so nothing the model did can have caused it.
    let baseline_at = events
        .iter()
        .position(|e| matches!(e, sc_core::AgentEvent::BaselineVerification { .. }))
        .unwrap();
    let first_turn_at = events
        .iter()
        .position(|e| matches!(e, sc_core::AgentEvent::ModelTurn { .. }))
        .unwrap();
    assert!(
        baseline_at < first_turn_at,
        "the baseline must be measured before the first model turn"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// Every observation the model was actually shown this run, in order.
///
/// `ToolResult` carries the observation AFTER the harness has appended its notes and steers,
/// which is the whole point: asserting on the steer means asserting on the bytes that reached
/// the model, not on a helper the loop might not have called.
fn observations(events: &[sc_core::AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            sc_core::AgentEvent::ToolResult { full, .. } => Some(full.clone()),
            _ => None,
        })
        .collect()
}

/// Every directive the stall ladder injected this run.
fn ladder_advice(events: &[sc_core::AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            sc_core::AgentEvent::Advice { advice, .. } => Some(advice.clone()),
            _ => None,
        })
        .collect()
}

fn run_observed(
    backend: &dyn ModelBackend,
    ws: &Path,
    cfg: &AgentConfig,
    task: &str,
) -> (sc_core::AgentReport, Vec<sc_core::AgentEvent>) {
    use std::sync::Mutex;
    let seen = Mutex::new(Vec::new());
    let sink = sc_core::FnSink(|e: &sc_core::AgentEvent| seen.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    let report =
        sc_core::run_agent_observed(backend, None, &registry, &ParseRepair, task, ws, cfg, &sink)
            .unwrap();
    (report, seen.into_inner().unwrap())
}

/// A senior that always has something encouraging and useless to say.
///
/// The 598s run had one configured, and that is what made the ladder so expensive: a
/// successful advisor consult resets the stall and returns `Recovered`, so the run buys
/// `ADVISOR_LIMIT` rounds of a full prompt pass plus a maximum-length T1 generation on top of
/// the `SELF_RECOVERY_LIMIT` rounds below it -- five interventions before `Stalled` was even
/// reachable. Without an advisor the self-recovery bound alone keeps a toy fixture short, so
/// a test that omits one cannot see the cost it is meant to measure.
struct Advisor;
impl ModelBackend for Advisor {
    fn name(&self) -> &str {
        "advisor"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        Ok(GenerateResponse::new(
            "Keep going, you are nearly there. Check the build once more.",
        ))
    }
}

fn run_observed_with_advisor(
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    ws: &Path,
    cfg: &AgentConfig,
    task: &str,
) -> (sc_core::AgentReport, Vec<sc_core::AgentEvent>) {
    use std::sync::Mutex;
    let seen = Mutex::new(Vec::new());
    let sink = sc_core::FnSink(|e: &sc_core::AgentEvent| seen.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    let report = sc_core::run_agent_observed(
        backend,
        advisor,
        &registry,
        &ParseRepair,
        task,
        ws,
        cfg,
        &sink,
    )
    .unwrap();
    (report, seen.into_inner().unwrap())
}

const DONE_STEER: &str = "THE WORK IS VERIFIED AND YOU ARE DONE";
const FINISHED_LADDER: &str = "The change is MADE and the verification is GREEN";

#[test]
fn a_verified_change_on_a_green_at_start_run_is_told_it_may_finish() {
    // THE MISSING POSITIVE SIGNAL. Phase 4 correctly stopped auto-finishing a green-at-start
    // run, but replaced it with a note that only ever says what green does NOT mean. A model
    // that has genuinely finished reads that as "check again" -- measured, eight times over,
    // for 246s of a 598s run.
    //
    // The two facts that make it safe to say "you are done" are both present here: the run
    // changed the workspace, and the verification after that change is green.
    let ws = green_repo();
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo extracted\n"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 6,
        ..config_with_verify()
    };

    let (report, events) = run_observed(&backend, &ws, &cfg, "Extract the net helpers.");

    assert_eq!(report.started_green, Some(true));
    let obs = observations(&events);
    let steered: Vec<&String> = obs.iter().filter(|o| o.contains(DONE_STEER)).collect();
    assert!(
        !steered.is_empty(),
        "a green verification after a real change must tell the model it may stop; \
         observations were: {obs:#?}"
    );
    let steer = steered[0];
    assert!(
        steer.contains("Call `finish` NOW"),
        "the steer must name the call to make, got: {steer}"
    );
    assert!(
        steer.contains("cannot tell you anything new"),
        "the steer must say re-running is pointless, got: {steer}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn nothing_changed_yet_still_gets_the_this_proves_nothing_note() {
    // The guard on the steer above. A model that calls `run_verification` before touching
    // anything must NOT be told it is done -- green there is just the state the workspace
    // arrived in, and crediting it is exactly the orphan-file false success Phase 4 removed.
    let ws = green_repo();
    let backend = Looping::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 4,
        ..config_with_verify()
    };

    let (_report, events) = run_observed(&backend, &ws, &cfg, "Extract the net helpers.");

    let obs = observations(&events);
    assert!(
        obs.iter()
            .any(|o| o.contains("you have not changed anything yet")),
        "an unchanged workspace must still be told green proves nothing; got {obs:#?}"
    );
    assert!(
        !obs.iter().any(|o| o.contains(DONE_STEER)),
        "a run that has changed NOTHING must never be told it is done"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_model_insisting_it_is_done_ends_the_run_instead_of_burning_the_budget() {
    // THE 598s REGRESSION, in miniature. The model makes its change, the build is green, and
    // then it re-runs the same check forever -- each reply appended to the prompt, each turn
    // slower than the last. Before the fix the ladder answered every repeat with advice and a
    // `stall.reset()`, so the cycle only ended at `max_steps`: on the real run, 45 turns of a
    // 35B model, 246s of them pure waste.
    //
    // Now: the ladder recognises the shape (green at start, workspace changed, last
    // verification green), says "call finish", and if the workspace still has not moved by
    // the next intervention it gives up rather than paying for a third round.
    let ws = green_repo();
    let backend = Looping::new(vec![
        r#"{"tool":"create_file","path":"net.sh","content":"echo extracted\n"}"#,
        // ...and from here on, the same no-op check, byte for byte, forever.
        r#"{"tool":"run_verification"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 40,
        ..config_with_verify()
    };

    let advisor = Advisor;
    let (report, events) = run_observed_with_advisor(
        &backend,
        Some(&advisor),
        &ws,
        &cfg,
        "Extract the net helpers.",
    );

    assert!(
        !matches!(report.stop_reason, sc_core::StopReason::BudgetExhausted),
        "the run must END, not run out the step budget; stopped {:?} after {} steps",
        report.stop_reason,
        report.steps
    );
    // MEASURED on this fixture: 19 steps and five ladder rounds before the fix (three advisor
    // consults, two self-recoveries, each one resetting the stall for another four turns), 10
    // steps and two rounds after. The ladder now needs only the repeat to trip
    // (`repeat_limit` 3), one intervention naming the real situation, and one more stall with
    // nothing changed. The real run never reached the end of that five-round ladder at all --
    // it hit the step cap first, at 45 turns.
    assert!(
        report.steps <= 12,
        "the run must end within a turn or two of the loop being detected, took {} of {} \
         steps (stopped {:?})",
        report.steps,
        cfg.max_steps,
        report.stop_reason
    );
    // And the advice it got named the real situation, not "take a concrete next action".
    let ladder = ladder_advice(&events);
    assert!(
        ladder.iter().any(|a| a.contains(FINISHED_LADDER)),
        "the ladder must tell a finished model it is finished, got: {ladder:#?}"
    );
    assert!(
        !ladder
            .iter()
            .any(|a| a.contains("you are stuck in a loop calling")),
        "the generic 'take a concrete next action' directive is wrong here: it asks a \
         finished model for another action, and another action IS another verification"
    );
    assert!(
        ladder.len() <= 2,
        "the ladder must not keep paying for rounds that change nothing, it fired {} times:          {ladder:#?}",
        ladder.len()
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_ladder_gives_up_when_its_advice_changes_nothing() {
    // The general rule behind the regression above, isolated from the green-at-start shape:
    // a model that ignores the harness twice, with no change to the workspace in between,
    // does not get a third round. Repeated reads are the purest form of it -- nothing about
    // a read can ever change the workspace, so no round can be the one that lands.
    let ws = red_repo();
    let backend = Looping::new(vec![r#"{"tool":"read_file","path":"impl.sh"}"#]);
    let cfg = AgentConfig {
        max_steps: 40,
        ..config_with_verify()
    };

    let report = run(&backend, &ws, &cfg);

    assert!(
        matches!(report.stop_reason, sc_core::StopReason::Stalled(_)),
        "advice that lands nowhere twice must stop the run, got {:?}",
        report.stop_reason
    );
    assert!(
        report.steps <= 12,
        "and it must stop promptly, took {} of {}",
        report.steps,
        cfg.max_steps
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_red_suite_is_never_told_to_call_finish() {
    // The TDD path is untouched. While the suite is still failing there is nothing verified
    // and nothing to finish: the model must see the failures, not permission to stop.
    let ws = red_repo();
    let backend = Looping::new(vec![
        // A real edit that does NOT fix the bug: bytes change, the suite stays red.
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"return 1; # no"}"#,
        r#"{"tool":"run_verification"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 8,
        ..config_with_verify()
    };

    let (report, events) = run_observed(&backend, &ws, &cfg, "Make is_even correct.");

    assert_eq!(report.started_green, Some(false));
    let obs = observations(&events);
    assert!(
        !obs.iter().any(|o| o.contains(DONE_STEER)),
        "a red-at-start run must never be told the work is verified; got {obs:#?}"
    );
    let ladder = ladder_advice(&events);
    assert!(
        !ladder.iter().any(|a| a.contains(FINISHED_LADDER)),
        "and the ladder must not tell it to finish either; got {ladder:#?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
