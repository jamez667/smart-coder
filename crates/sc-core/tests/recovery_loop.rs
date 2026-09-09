//! M4 exit-criterion test (spec 07): the agent recovers from induced failures
//! (bad edit, failing test, repeated action) **without human rescue**, or
//! escalates cleanly — and when a senior advisor is present, a nudge gets it
//! unstuck (spec 02 "junior asks senior").

use std::cell::RefCell;
use std::path::Path;

use sc_core::{run_agent_recovering, AgentConfig, AgentEvent, EventSink, ParseRepair, StopReason};
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

/// An [`EventSink`] that keeps every `ToolResult`, paired with the `ToolCall` it answered
/// — so a test can assert what the harness actually TOLD the model, not just how the run
/// ended.
#[derive(Default)]
struct Collect {
    // (tool, observation), in order.
    results: std::sync::Mutex<Vec<(String, String)>>,
    pending: std::sync::Mutex<Option<String>>,
}
impl Collect {
    /// Every observation the named tool produced this run.
    fn tool_results(&self, tool: &str) -> Vec<String> {
        self.results
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, o)| o.clone())
            .collect()
    }
}
impl EventSink for Collect {
    fn record(&self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolCall { tool, .. } => {
                *self.pending.lock().unwrap() = Some(tool.clone());
            }
            AgentEvent::ToolResult { full, .. } => {
                if let Some(tool) = self.pending.lock().unwrap().take() {
                    self.results.lock().unwrap().push((tool, full.clone()));
                }
            }
            _ => {}
        }
    }
}

/// **A model repeating a no-op edit must be TOLD it is a no-op, and must stall.**
///
/// THE MEASURED COST. `edit_file` answered "ok (1 replacement)" for an edit whose
/// `old_str` equalled its `new_str` — it never compared the result to what it started
/// with. On one Mellum run four turns were verbatim no-ops, each answered "ok", and the
/// model spent the rest of a 313-second run reasoning from a false premise.
///
/// Three things have to hold, and this pins all three:
///
/// 1. **The observation is honest.** Every one of those turns must say "no-op", not "ok".
///    This is the half the fix changes; without the guard the assertions below on the
///    ToolResult text fail with the exact live lie, `edit_file impl.sh ok (1 replacement)`.
/// 2. **The write does not count as a workspace change.** `changed` is
///    `Journal::snapshot` before vs. after — file CONTENT — so a no-op was ALREADY
///    `changed == false` before the fix, and skipping the write keeps it that way. Pinned
///    because several things key off `changed` (the auto-verify, the stall detector's
///    no-progress count, `verify_fresh.touched`, `made_a_change`); if a no-op ever marked
///    the workspace dirty it would reset the stall detector every turn and re-verify for
///    nothing, which is how a model loops on no-ops forever.
/// 3. **The loop ends.** Identical calls hash identically (`action_hash` over tool + key
///    arg), so the repeat detector sees it and the recovery ladder stops the run — here in
///    9 steps against a 30-step budget.
#[test]
fn a_repeated_no_op_edit_is_reported_as_one_and_stalls() {
    let ws = temp("noop-loop");
    let f = ws.join("impl.sh");
    let original = "is_even() { return 1; }\n";
    std::fs::write(&f, original).unwrap();

    // The exact shape of the live failure: old_str == new_str, submitted over and over.
    let no_op =
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"return 1;"}"#;
    let backend = Scripted::new(vec![no_op]);
    let cfg = AgentConfig {
        max_steps: 30,
        repeat_limit: 3,
        ..Default::default()
    };
    let sink = Collect::default();
    let report = sc_core::run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &ParseRepair,
        "fix it",
        &ws,
        &cfg,
        &sink,
    )
    .unwrap();

    // 1. Every edit_file turn told the model the truth.
    let edits = sink.tool_results("edit_file");
    assert!(!edits.is_empty(), "the model did attempt edits");
    for o in &edits {
        assert!(
            o.contains("no-op") && o.contains("nothing written"),
            "every no-op turn must say so, got: {o}"
        );
        assert!(
            !o.contains("1 replacement"),
            "the harness must never claim a replacement landed: {o}"
        );
    }

    // 2. Nothing was written, and the run registers no workspace change.
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        original,
        "a no-op edit must leave the file exactly as it was"
    );
    assert!(
        report.change_summary.contains("no files changed"),
        "a no-op must not register as a workspace change: {}",
        report.change_summary
    );

    // 3. The loop ended on the stall ladder, well inside the budget.
    assert!(!report.finished);
    assert!(
        matches!(report.stop_reason, StopReason::Stalled(_)),
        "a loop of no-ops must end stalled, got {:?} after {} steps",
        report.stop_reason,
        report.steps
    );
    assert!(
        report.steps < 30,
        "must stop well under the budget, took {}",
        report.steps
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// **A mislabelled tool call runs as the tool its ARGUMENTS name, all the way through the loop.**
///
/// THE HARNESS CAUSED THIS ONE. Observed live: the loop told the model *"STOP editing by anchor.
/// Instead call `write_file` with `path` `pathfind.rs` and the ENTIRE corrected file contents in
/// one shot."* The model obeyed the NAME and kept its own intent, sending
/// `{"tool":"write_file","path":…,"old_str":…,"new_str":…}` — a complete, unambiguous
/// `edit_file` wearing the wrong label. Validation answered `tool "write_file" has no parameter
/// "new_str"` and the whole turn was discarded. Rejecting it teaches the model nothing: it did
/// exactly what it was told.
///
/// The strategy-level recovery is pinned in `strategy::tests`. What this pins is the part that
/// only the loop can prove: the recovered call carries the RECOVERED name, so everything keyed
/// off `call.name` stays right —
///
/// 1. the `ToolCall` event (and therefore the run log / activity feed) says `edit_file`;
/// 2. `mutating_path` finds `edit_file`'s Mutating side effect, so the journal snapshots the
///    path and the edit is recorded as a real workspace change; and
/// 3. the edit actually lands in the file, which is what the model asked for.
///
/// Without the rung the turn is thrown away and the file is untouched.
#[test]
fn an_edit_labelled_write_file_runs_as_an_edit_through_the_whole_loop() {
    let ws = temp("mislabelled");
    let f = ws.join("pathfind.rs");
    std::fs::write(&f, "/// 4-connected neighbours.\nfn go() {}\n").unwrap();

    let backend = Scripted::new(vec![
        // The live payload: named write_file, shaped exactly like an edit_file.
        r#"{"new_str":"/// 8-connected neighbours.","old_str":"/// 4-connected neighbours.","path":"pathfind.rs","tool":"write_file"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let sink = Collect::default();
    let report = sc_core::run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &ParseRepair,
        "fix it",
        &ws,
        &AgentConfig::default(),
        &sink,
    )
    .unwrap();

    // 3. The edit landed.
    let after = std::fs::read_to_string(&f).unwrap();
    assert!(
        after.starts_with("/// 8-connected neighbours."),
        "the anchored edit must actually be applied, got: {after:?}"
    );

    // 1. The loop saw an `edit_file`, not the `write_file` the model named — the ToolCall
    //    event, the action hash and `key_arg` all read from the same `call.name`.
    let edits = sink.tool_results("edit_file");
    assert_eq!(
        edits.len(),
        1,
        "the turn must be attributed to edit_file, got results: {:?}",
        sink.results.lock().unwrap()
    );
    assert!(
        edits[0].contains("1 replacement"),
        "and it must be the real anchored edit: {}",
        edits[0]
    );
    assert!(
        sink.tool_results("write_file").is_empty(),
        "nothing may be attributed to the name the model wrote"
    );

    // 2. `mutating_path` resolved through the RECOVERED name, so the journal recorded it.
    assert!(
        report.change_summary.contains("pathfind.rs"),
        "the journal must record the change: {}",
        report.change_summary
    );

    // The note the loop appends so the model learns the right name.
    assert_eq!(
        sc_core::mislabelled_tool_recovery(
            r#"{"new_str":"b","old_str":"a","path":"pathfind.rs","tool":"write_file"}"#,
            &default_registry(),
        ),
        Some(("write_file".to_string(), "edit_file".to_string())),
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// The model is TOLD when its call ran under a different name than it gave.
///
/// The harness causes this one: the rewrite escalation orders `write_file`, and a
/// model that wants an anchored edit obeys the name while sending old_str/new_str.
/// Running it as the edit it is fixes the turn; without the note the model never
/// learns the right name and does it again.
#[test]
fn a_mislabelled_call_tells_the_model_what_it_actually_ran() {
    let ws = temp("mislabelled-note");
    std::fs::write(
        ws.join("a.rs"),
        "fn go() { let x = 1; }
",
    )
    .unwrap();

    let sink = Collect::default();
    // Names write_file, hands over edit_file's arguments.
    let backend = Scripted::new(vec![
        r#"{"tool":"write_file","path":"a.rs","old_str":"let x = 1;","new_str":"let x = 2;"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    sc_core::run_agent_observed(
        &backend,
        None,
        &default_registry(),
        &ParseRepair,
        "change x to 2",
        &ws,
        &AgentConfig {
            max_steps: 2,
            ..AgentConfig::default()
        },
        &sink,
    )
    .unwrap();

    let seen = sink.tool_results("edit_file").join(
        "
",
    );
    assert!(
        seen.contains("you named write_file") && seen.contains("edit_file"),
        "the model must be told which tool actually ran, got: {seen}"
    );
    let after = std::fs::read_to_string(ws.join("a.rs")).unwrap();
    assert!(after.contains("let x = 2;"), "the edit must apply: {after}");

    let _ = std::fs::remove_dir_all(&ws);
}
