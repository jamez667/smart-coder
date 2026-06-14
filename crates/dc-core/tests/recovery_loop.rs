//! M4 exit-criterion test (spec 07): the agent recovers from induced failures
//! (bad edit, failing test, repeated action) **without human rescue**, or
//! escalates cleanly — and when a senior advisor is present, a nudge gets it
//! unstuck (spec 02 "junior asks senior").

use std::cell::RefCell;
use std::path::Path;

use dc_core::{run_agent_recovering, AgentConfig, ParseRepair, StopReason};
use dc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use dc_proto::Result;
use dc_tools::default_registry;

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
        Ok(GenerateResponse { content })
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dc-core-recov-{tag}-{}-{}",
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
) -> dc_core::AgentReport {
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
fn ask_user_with_no_advisor_escalates_cleanly() {
    // No senior to ask -> a clean Escalated stop carrying the question.
    let ws = temp("ask-none");
    let backend = Scripted::new(vec![r#"{"tool":"ask_user","question":"what now?"}"#]);
    let report = run(&backend, None, &ws, &AgentConfig::default());

    assert!(!report.finished);
    match report.stop_reason {
        StopReason::Escalated(q) => assert!(q.contains("what now?")),
        other => panic!("expected Escalated, got {other:?}"),
    }
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
