//! M4 exit-criterion test (spec 07): the agent recovers from induced failures
//! (bad edit, failing test, repeated action) **without human rescue**, or
//! escalates cleanly — and when a senior advisor is present, a nudge gets it
//! unstuck (spec 02 "junior asks senior").

use std::cell::RefCell;
use std::path::Path;

use sc_core::{run_agent_recovering, AgentConfig, ParseRepair, StopReason};
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use sc_tools::default_registry;

/// A backend that replays a fixed script, repeating the LAST entry forever once
/// the script runs out — so we can model an agent that gets stuck.
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
        let content = if s.len() > 1 {
            s.remove(0)
        } else {
            s.first()
                .cloned()
                .unwrap_or_else(|| r#"{"tool":"finish"}"#.to_string())
        };
        Ok(GenerateResponse::new(content))
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-recov-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    ws: &Path,
    cfg: &AgentConfig,
) -> sc_core::AgentReport {
    let registry = default_registry();
    run_agent_recovering(backend, advisor, &registry, &ParseRepair, "fix it", ws, cfg).unwrap()
}

#[test]
fn recovers_from_a_bad_edit_then_a_correct_one() {
    // The model first attempts an anchored edit that doesn't match (induced
    // failure), observes the error, then makes the right edit and finishes.
    let ws = temp("bad-edit");
    std::fs::write(ws.join("impl.sh"), "is_even() { return 1; }\n").unwrap();

    let backend = Scripted::new(vec![
        // Bad anchor — won't match, error fed back.
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"NOPE","new_str":"x"}"#,
        // Correct edit.
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"[ $(( $1 % 2 )) -eq 0 ];"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let report = run(&backend, None, &ws, &AgentConfig::default());

    assert!(report.finished, "should recover and finish");
    assert!(report.change_summary.contains("impl.sh"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn stalls_cleanly_with_no_advisor_when_looping() {
    // The model loops forever on the same no-op read. With no advisor, the harness
    // detects the loop and stops cleanly with a Stalled reason — no infinite run.
    let ws = temp("stall");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let backend = Scripted::new(vec![r#"{"tool":"read_file","path":"a.txt"}"#]);
    let cfg = AgentConfig {
        max_steps: 20,
        repeat_limit: 3,
        ..Default::default()
    };
    let report = run(&backend, None, &ws, &cfg);

    assert!(!report.finished);
    assert!(
        matches!(report.stop_reason, StopReason::Stalled(_)),
        "{:?}",
        report.stop_reason
    );
    // It stopped at the loop threshold, well before the step budget.
    assert!(
        report.steps < 20,
        "should stop early, took {}",
        report.steps
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_advisor_nudge_breaks_a_loop_and_lets_it_finish() {
    // The model loops on a no-op read; once the advisor nudges it, the (scripted)
    // model "takes the hint" and finishes. We model the hint working by having the
    // script's tail be a finish after enough turns.
    let ws = temp("nudge");
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    // read, read, read (trips loop at 3) -> advisor nudge -> finish.
    let backend = Scripted::new(vec![
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let advisor = Scripted::new(vec!["Stop re-reading; the file is fine. Just finish."]);

    let cfg = AgentConfig {
        max_steps: 12,
        repeat_limit: 3,
        ..Default::default()
    };
    let report = run(&backend, Some(&advisor), &ws, &cfg);

    assert!(
        report.finished,
        "advisor nudge should let it finish: {:?}",
        report.stop_reason
    );
    assert!(
        report.interventions >= 1,
        "the advisor should have been consulted"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **An edit that leaves the same test red is not progress.**
///
/// Every edit changes bytes, and a workspace change used to reset the stall detector
/// outright -- so a model alternating two useless edits, each followed by the same red
/// auto-verify, could never stall and always ran to the step budget. Now the harness
/// hashes the failure after each auto-verify; three identical failures in a row are
/// reported to the detector as non-progress, and the recovery ladder runs and gives up.
#[test]
fn edits_that_keep_the_same_test_red_stall_instead_of_exhausting_the_budget() {
    let ws = temp("same-failure");
    std::fs::write(ws.join("impl.sh"), "is_even() { return 1; }\n").unwrap();
    std::fs::write(
        ws.join("test.sh"),
        ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
    )
    .unwrap();

    // Two edits that each change the file and neither of which fixes anything, alternating
    // so the action hash never repeats and every turn is a genuine workspace change.
    let to_two =
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"return 2;"}"#;
    let to_one =
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 2;","new_str":"return 1;"}"#;
    let backend = Scripted::new(vec![
        to_two, to_one, to_two, to_one, to_two, to_one, to_two, to_one, to_two, to_one, to_two,
        to_one,
    ]);
    let cfg = AgentConfig {
        max_steps: 12,
        verify_command: Some("sh test.sh".to_string()),
        ..Default::default()
    };
    let report = run(&backend, None, &ws, &cfg);

    assert!(!report.finished);
    assert!(
        matches!(report.stop_reason, StopReason::Stalled(_)),
        "the unchanged failure should end the run stalled, got {:?} after {} steps",
        report.stop_reason,
        report.steps
    );
    assert!(
        report.steps < 12,
        "should stop before the budget, took {}",
        report.steps
    );
    // The ladder ran (self-recovery directives) before it gave up.
    assert!(report.interventions >= 1, "{report:?}");
    // `verified` is what the run log saw last (red), not a fresh run of the suite.
    assert_eq!(report.verified, Some(false));
    let _ = std::fs::remove_dir_all(&ws);
}

/// A stop report's `verified` comes from the run log. When the suite never ran -- a
/// verify command was configured but no edit ever triggered it -- the answer is `None`,
/// not a re-run at stop time.
#[test]
fn a_stalled_run_that_never_verified_reports_verified_as_none() {
    let ws = temp("stall-unverified");
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    std::fs::write(ws.join("test.sh"), "exit 1\n").unwrap();

    let backend = Scripted::new(vec![r#"{"tool":"read_file","path":"a.txt"}"#]);
    let cfg = AgentConfig {
        max_steps: 20,
        repeat_limit: 3,
        verify_command: Some("sh test.sh".to_string()),
        ..Default::default()
    };
    let report = run(&backend, None, &ws, &cfg);

    assert!(matches!(report.stop_reason, StopReason::Stalled(_)));
    assert_eq!(report.verified, None, "{report:?}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn ask_user_consults_the_advisor_and_continues() {
    // The model explicitly asks for help, gets advice, then finishes — escalation
    // is a nudge, not a stop, when an advisor is present.
    let ws = temp("ask");
    let backend = Scripted::new(vec![
        r#"{"tool":"ask_user","question":"which file holds the bug?"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let advisor = Scripted::new(vec!["Look in impl.sh first."]);
    let report = run(&backend, Some(&advisor), &ws, &AgentConfig::default());

    assert!(report.finished);
    assert!(report.interventions >= 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn ask_user_with_no_advisor_is_told_to_decide_and_continues() {
    // No senior to ask. The run used to stop dead here (`Escalated`), throwing away
    // everything built up over a question the model could usually settle itself. Now the
    // harness answers in-band -- "no one is available, decide for yourself" -- counts the
    // intervention, and the model carries on.
    use sc_core::select_strategy;
    use sc_core::{run_agent_observed, AgentEvent, FnSink};
    use std::sync::Mutex;

    let ws = temp("ask-none");
    let backend = Scripted::new(vec![
        r#"{"tool":"ask_user","question":"what now?"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    let strategy = select_strategy(&backend.capabilities());
    let report = run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        "fix it",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    assert!(report.finished, "{:?}", report.stop_reason);
    assert_eq!(report.interventions, 1);
    let evs = events.lock().unwrap();
    let told = evs.iter().any(|e| {
        matches!(e, AgentEvent::ToolResult { full, .. } if full.contains("No one is available to answer"))
    });
    assert!(
        told,
        "the model should be told to decide for itself: {evs:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn plan_first_produces_a_plan_and_still_finishes() {
    // With plan_first, the same backend is asked to plan, then act. A scripted
    // backend returns a plan array first, then tool calls.
    let ws = temp("plan");
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let backend = Scripted::new(vec![
        r#"["read the file", "finish up"]"#, // the plan
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        plan_first: true,
        ..Default::default()
    };
    let report = run(&backend, None, &ws, &cfg);
    assert!(report.finished);
    let _ = std::fs::remove_dir_all(&ws);
}

/// Reading DIFFERENT files is a search, and must not be nudged.
#[test]
fn reading_several_different_files_is_not_a_repeat() {
    let ws = temp("distinct-reads");
    for n in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(ws.join(n), "contents").unwrap();
    }
    let backend = Scripted::new(vec![
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"read_file","path":"b.txt"}"#,
        r#"{"tool":"read_file","path":"c.txt"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 12,
        repeat_limit: 99,
        ..Default::default()
    };
    let report = run(&backend, None, &ws, &cfg);
    assert_eq!(
        report.interventions, 0,
        "distinct reads are a search, not a loop: {report:?}"
    );
}
