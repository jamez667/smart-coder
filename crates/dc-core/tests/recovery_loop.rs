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
fn idempotent_nudge_names_write_file_and_supersedes_the_stale_result() {
    // When a model repeats an idempotent call (read/list) with no advisor, the harness
    // injects a text NUDGE. This test pins the two fixes that make the nudge actually land
    // on a coder model (it was ignored 30/30 times on the live scale ladder):
    //   Fix #1 — the nudge names the APPLICABLE tool (`write_file` to create the missing
    //            file), not `edit_file` on a file that doesn't exist yet.
    //   Fix #2 — the PRIOR turn's successful result of the same call is superseded in the
    //            window, so the nudge isn't drowned by a visible "it worked".
    use std::sync::Mutex;
    use dc_core::{run_agent_observed, AgentEvent, FnSink};
    use dc_core::select_strategy;

    let ws = temp("nudge-text");
    std::fs::write(ws.join("a.txt"), "hello-from-a-txt").unwrap();

    // read, read (the 2nd trips the dedup nudge), then a productive write. repeat_limit is
    // high so the stall STOP can't fire first — we're isolating the nudge path.
    let backend = Scripted::new(vec![
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"read_file","path":"a.txt"}"#,
        r#"{"tool":"write_file","path":"out.py","content":"x = 1\n"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 10,
        repeat_limit: 8,
        no_progress_limit: 8,
        // Verbose so we can inspect the assembled window and confirm Fix #2 superseded the
        // stale identical result (it lives in `recent`, surfaced via PromptAssembled).
        verbose: true,
        ..Default::default()
    };

    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    let registry = default_registry();
    let strategy = select_strategy(&backend.capabilities());
    let report = run_agent_observed(
        &backend, None, &registry, strategy.as_ref(), "fix it", &ws, &cfg, &sink,
    )
    .unwrap();

    // The model reached the productive write (it wasn't stalled out before acting).
    assert!(
        report.change_summary.contains("out.py"),
        "should reach the write_file, got change: {:?}",
        report.change_summary
    );

    let evs = events.lock().unwrap();
    // Fix #1: the injected nudge observation names write_file, NOT the impossible edit_file.
    let nudge = evs.iter().find_map(|e| match e {
        AgentEvent::ToolResult { full, .. } if full.contains("re-running it changes nothing") => {
            Some(full.clone())
        }
        _ => None,
    });
    let nudge = nudge.expect("a dedup nudge should have been injected");
    assert!(
        nudge.contains("write_file"),
        "nudge must name write_file (Fix #1), got: {nudge}"
    );
    assert!(
        !nudge.contains("now with edit_file"),
        "nudge must NOT prescribe the old edit_file-only wording: {nudge}"
    );
    // Fix #2: the prior identical result was superseded IN THE WINDOW. The marker lives in
    // `recent`, so it shows up in a later assembled prompt — and crucially the original
    // successful body ("hello-from-a-txt") must NOT still be sitting next to the nudge.
    let superseded_in_prompt = evs.iter().any(|e| matches!(
        e,
        AgentEvent::PromptAssembled { messages, .. }
            if messages.iter().any(|m| m.content.contains("superseded"))
    ));
    assert!(
        superseded_in_prompt,
        "the prior identical result should be superseded in the assembled window (Fix #2)"
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
