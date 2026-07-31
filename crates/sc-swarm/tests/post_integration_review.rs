//! Spec 16 end-to-end: post-integration review inside a real swarm run.
//!
//! The unit tests prove each piece in isolation. This proves the wiring — that a
//! review actually runs over the diff that landed, that its findings reach the
//! event stream and the report, and above all that the two rules survive the trip
//! through the orchestrator:
//!
//! 1. An uncorroborated finding can never stop or redirect a run.
//! 2. A subtask that passes its tests is never failed by a reviewer.
//!
//! Scripted backends throughout — no live model, per spec 11 and this repo's own
//! practice.

use std::sync::Mutex;

use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result;
use sc_swarm::{
    run_swarm_board, FnSwarmSink, ReviewAction, ReviewConfig, Severity, Subtask, SwarmConfig,
    SwarmEvent, TaskBoard,
};

/// A backend that answers by matching a substring of the prompt — one instance
/// plays orchestrator, worker and reviewer, keyed on what each is asked.
struct Scripted {
    name: String,
    replies: Mutex<Vec<(String, Vec<String>)>>,
}

impl Scripted {
    fn new(name: &str, replies: Vec<(&str, Vec<&str>)>) -> Self {
        Self {
            name: name.to_string(),
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
                    .collect(),
            ),
        }
    }
}

impl ModelBackend for Scripted {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 32_768,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        let prompt = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut replies = self.replies.lock().unwrap();
        for (key, queue) in replies.iter_mut() {
            if prompt.contains(key.as_str()) && !queue.is_empty() {
                return Ok(GenerateResponse {
                    content: queue.remove(0),
                });
            }
        }
        // A reviewer that isn't scripted found nothing; a worker that isn't
        // scripted proposes nothing new.
        Ok(GenerateResponse {
            content: "[]".to_string(),
        })
    }
}

fn temp_repo(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-swarm-review-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A repo with an existing `format_date` in `src/utils/date.rs`, and a worker
/// about to reimplement it in `src/report/render.rs` — the scenario spec 16 is
/// built around. The worker cannot see `date.rs`; the reviewer can.
fn duplicating_repo(tag: &str) -> (std::path::PathBuf, String) {
    let ws = temp_repo(tag);
    std::fs::create_dir_all(ws.join("src/utils")).unwrap();
    std::fs::create_dir_all(ws.join("src/report")).unwrap();
    std::fs::write(
        ws.join("src/utils/date.rs"),
        "fn unrelated() {\n    let _ = 1;\n}\n\
         fn format_date(d: u64) -> String {\n    String::new()\n}\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("src/report/render.rs"),
        "fn render() -> String {\n    String::new()\n}\n",
    )
    .unwrap();

    // What the worker proposes and what the orchestrator merges: a fresh
    // `format_date` right next to `render`, inserted mid-file.
    let merged = "fn render() -> String {\n    format_date(0)\n}\n\
                  fn format_date(d: u64) -> String {\n    let s = String::new();\n    s\n}\n";
    (ws, merged.to_string())
}

fn board() -> TaskBoard {
    TaskBoard::new(vec![
        Subtask::new("t1", "render a report line").with_files(vec!["src/report/render.rs".into()])
    ])
}

fn review_cfg(action: ReviewAction) -> ReviewConfig {
    ReviewConfig {
        enabled: true,
        min_changed_lines: 1,
        action,
        gate_at: Severity::High,
        ..Default::default()
    }
}

/// Collect every event a run emits.
fn run(
    ws: &std::path::Path,
    backend: &Scripted,
    reviewer: &Scripted,
    cfg: SwarmConfig,
) -> (sc_swarm::SwarmReport, Vec<SwarmEvent>) {
    let log: Mutex<Vec<SwarmEvent>> = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| log.lock().unwrap().push(e.clone()));
    let report = run_swarm_board(
        backend,
        backend,
        // The reviewer runs on the advisor/T1 backend, not a worker (spec 16).
        Some(reviewer as &(dyn ModelBackend + Sync)),
        board(),
        ws,
        &cfg,
        &sink,
    );
    let events = log.into_inner().unwrap();
    (report, events)
}

/// The worker + orchestrator script for the duplicating scenario.
fn coder(merged: &str) -> Scripted {
    Scripted::new(
        "worker",
        vec![
            ("render a report line", vec![merged, merged, merged]),
            ("File: src/report/render.rs", vec![merged, merged, merged]),
        ],
    )
}

#[test]
fn a_corroborated_duplicate_redispatches_the_subtask_with_actionable_evidence() {
    // The highest-value outcome and the reason the whole spec is worth building:
    // the index found the original while building the review prompt, the model
    // agreed, and the worker is re-dispatched with a named target rather than
    // "you duplicated something".
    let (ws, merged) = duplicating_repo("retry");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change reimplement",
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                     "severity":"high","summary":"this already lives in utils"}]"#,
            ],
        )],
    );

    let (report, events) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            review: review_cfg(ReviewAction::Retry),
            ..Default::default()
        },
    );

    // The finding reached the stream, corroborated, with its evidence.
    let finding = events
        .iter()
        .find_map(|e| match e {
            SwarmEvent::ReviewFinding {
                corroborated,
                evidence,
                anchor,
                ..
            } => Some((*corroborated, evidence.clone(), anchor.clone())),
            _ => None,
        })
        .expect("a ReviewFinding was emitted");
    assert!(finding.0, "the index found the original, so it may act");
    let evidence = finding.1.expect("corroboration carries what it found");
    assert!(evidence.contains("format_date"), "{evidence}");
    assert!(
        evidence.contains("src/utils/date.rs:4"),
        "the symbol AND its location: {evidence}"
    );
    assert_eq!(finding.2.symbol.as_deref(), Some("format_date"));

    // And it drove a retry — the subtask went back to a worker.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SwarmEvent::SubtaskRetry { .. })),
        "a corroborated finding re-dispatches the subtask"
    );

    // The subtask is still Done: review redirects work, it never fails it.
    assert_eq!(report.done, 1, "{report:?}");
    assert_eq!(report.failed, 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_uncorroborated_finding_is_reported_but_never_retries_or_gates() {
    // A high-severity opinion the index cannot corroborate. It must ride the
    // stream and the report — and change nothing about the run.
    let (ws, merged) = duplicating_repo("opinion");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change match how the surrounding",
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"render",
                     "severity":"high","summary":"I'd have used a builder here"}]"#,
            ],
        )],
    );

    let (report, events) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            // Even in the most interventionist mode.
            review: review_cfg(ReviewAction::Retry),
            ..Default::default()
        },
    );

    let (corroborated, evidence) = events
        .iter()
        .find_map(|e| match e {
            SwarmEvent::ReviewFinding {
                corroborated,
                evidence,
                ..
            } => Some((*corroborated, evidence.clone())),
            _ => None,
        })
        .expect("the opinion is still reported");
    assert!(!corroborated);
    assert!(
        evidence.is_none(),
        "no check spoke, so there is no evidence"
    );

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SwarmEvent::SubtaskRetry { .. })),
        "an opinion never re-dispatches a subtask"
    );
    assert_eq!(
        report.blocking_findings, 0,
        "and never counts toward stopping the run"
    );
    assert!(report.all_done);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_subtask_that_passes_its_tests_is_never_failed_by_a_reviewer() {
    // Spec 16, "Budgets, and the last retry": green tests + a failed review on the
    // last allowed attempt GATES, it does not fail. The work is verified correct,
    // and discarding it over an unfixed finding is the worse outcome.
    let (ws, merged) = duplicating_repo("lastretry");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change reimplement",
            // The finding stands on every attempt — the worker never fixes it.
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                     "severity":"high","summary":"duplicate"}]"#,
            ],
        )],
    );

    let (report, events) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            // No retry budget at all: the first review IS the last attempt.
            max_subtask_retries: 0,
            review: review_cfg(ReviewAction::Retry),
            ..Default::default()
        },
    );

    // Done, not Failed — with the finding attached rather than dropped.
    assert_eq!(report.done, 1, "{report:?}");
    assert_eq!(report.failed, 0, "a reviewer may not fail green work");
    assert!(report.all_done);
    assert_eq!(report.blocking_findings, 1, "but a human should look");
    assert_eq!(
        report.findings.len(),
        1,
        "the finding rides along, never silently accepted"
    );
    assert_eq!(report.findings[0].0, "t1");

    // And the stream says so too, with the blocking count carried rather than
    // left for a renderer to recompute.
    let finished = events
        .iter()
        .find_map(|e| match e {
            SwarmEvent::ReviewFinished {
                findings, blocking, ..
            } => Some((*findings, *blocking)),
            _ => None,
        })
        .expect("ReviewFinished was emitted");
    assert_eq!(finished, (1, 1));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_review_retry_that_goes_nowhere_keeps_the_green_result_rather_than_failing_it() {
    // The subtle case, and the one that quietly destroys work if it is got wrong.
    //
    // A review-driven retry differs in kind from a test-driven one: the subtask is
    // ALREADY green and integrated, so the retry is speculative improvement. The
    // worker here re-proposes exactly what it proposed before — so the second merge
    // produces no change and the integration is rejected. Treating that like an
    // ordinary exhausted retry would mark a verified-correct subtask Failed and
    // revert it, throwing away green code over a style finding: precisely the
    // outcome spec 16 names as the worse one.
    let (ws, merged) = duplicating_repo("stubborn");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change reimplement",
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                     "severity":"high","summary":"duplicate"}]"#,
            ],
        )],
    );

    let (report, events) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            // Budget for the review retry, which will get nowhere.
            max_subtask_retries: 2,
            review: review_cfg(ReviewAction::Retry),
            ..Default::default()
        },
    );

    assert!(
        events
            .iter()
            .any(|e| matches!(e, SwarmEvent::SubtaskRetry { .. })),
        "the retry did fire"
    );
    assert_eq!(report.failed, 0, "and failed nothing: {report:?}");
    assert_eq!(report.done, 1);
    assert!(report.all_done);
    // The work is still on disk — not reverted.
    let landed = std::fs::read_to_string(ws.join("src/report/render.rs")).unwrap();
    assert!(
        landed.contains("format_date"),
        "kept, not reverted: {landed}"
    );
    // And the finding is still reported rather than lost in the retry.
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.blocking_findings, 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn gate_mode_stops_the_run_at_a_human_checkpoint_without_reverting_anything() {
    // `--review-action gate`: a corroborated finding at or above the gating
    // severity stops the run for a human. What must NOT happen is a stop that
    // discards work — the subtask is completed and recorded first, then the human
    // is asked, so stopping costs nothing that already landed.
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StopGate {
        asked: AtomicUsize,
    }
    impl sc_swarm::ReviewGate for StopGate {
        fn checkpoint(
            &self,
            _subtask: &str,
            findings: &[sc_swarm::Finding],
            blocking: usize,
        ) -> sc_swarm::Checkpoint {
            self.asked.fetch_add(1, Ordering::SeqCst);
            // The human is handed the findings themselves, not just a count.
            assert!(!findings.is_empty());
            assert!(blocking >= 1);
            sc_swarm::Checkpoint::Stop
        }
    }

    let (ws, merged) = duplicating_repo("gate");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change reimplement",
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                     "severity":"high","summary":"duplicate"}]"#,
            ],
        )],
    );

    let gate = StopGate {
        asked: AtomicUsize::new(0),
    };
    let log: Mutex<Vec<SwarmEvent>> = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| log.lock().unwrap().push(e.clone()));
    let report = sc_swarm::run_swarm_board_gated(
        &backend,
        &backend,
        Some(&reviewer as &(dyn ModelBackend + Sync)),
        board(),
        &ws,
        &SwarmConfig {
            review: review_cfg(ReviewAction::Gate),
            ..Default::default()
        },
        &sink,
        &gate,
    );

    assert_eq!(gate.asked.load(Ordering::SeqCst), 1, "the human was asked");
    assert!(report.stopped_at_checkpoint, "and the run stopped");
    assert!(
        !report.all_done,
        "a run stopped at a checkpoint is not all-done: {report:?}"
    );
    // Stopping is not reverting, and it is not failing.
    assert_eq!(report.done, 1);
    assert_eq!(report.failed, 0);
    let landed = std::fs::read_to_string(ws.join("src/report/render.rs")).unwrap();
    assert!(landed.contains("format_date"), "kept: {landed}");
    assert_eq!(report.blocking_findings, 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_headless_gate_completes_the_run_and_still_reports_the_findings() {
    // Where no human is available (the default `AutoContinue`), gate mode does not
    // hang and does not silently drop: the run completes and the findings are
    // reported loudly (spec 16).
    let (ws, merged) = duplicating_repo("headless");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change reimplement",
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                     "severity":"high","summary":"duplicate"}]"#,
            ],
        )],
    );

    let (report, _) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            review: review_cfg(ReviewAction::Gate),
            ..Default::default()
        },
    );

    assert!(!report.stopped_at_checkpoint, "nobody to stop for");
    assert!(report.all_done);
    assert_eq!(report.blocking_findings, 1, "but reported, never dropped");
    assert_eq!(report.findings.len(), 1);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_uncorroborated_finding_never_reaches_the_checkpoint() {
    // The human is asked only about findings a deterministic check agreed with.
    // Waking someone up for a model's opinion is how a gate gets switched off.
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingGate(AtomicUsize);
    impl sc_swarm::ReviewGate for CountingGate {
        fn checkpoint(
            &self,
            _s: &str,
            _f: &[sc_swarm::Finding],
            _b: usize,
        ) -> sc_swarm::Checkpoint {
            self.0.fetch_add(1, Ordering::SeqCst);
            sc_swarm::Checkpoint::Stop
        }
    }

    let (ws, merged) = duplicating_repo("nogate");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION: Does this change match how the surrounding",
            vec![
                r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"render",
                     "severity":"high","summary":"I'd have used a builder"}]"#,
            ],
        )],
    );

    let gate = CountingGate(AtomicUsize::new(0));
    let log: Mutex<Vec<SwarmEvent>> = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| log.lock().unwrap().push(e.clone()));
    let report = sc_swarm::run_swarm_board_gated(
        &backend,
        &backend,
        Some(&reviewer as &(dyn ModelBackend + Sync)),
        board(),
        &ws,
        &SwarmConfig {
            review: review_cfg(ReviewAction::Gate),
            ..Default::default()
        },
        &sink,
        &gate,
    );

    assert_eq!(gate.0.load(Ordering::SeqCst), 0, "never asked");
    assert!(!report.stopped_at_checkpoint);
    assert!(report.all_done);
    // Still reported, just never able to stop anything.
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.blocking_findings, 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn review_is_off_by_default_and_costs_nothing() {
    let (ws, merged) = duplicating_repo("off");
    let backend = coder(&merged);
    // A reviewer that WOULD find something, if it were ever asked.
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION:",
            vec![r#"[{"hunk":"H0","file":"src/report/render.rs","summary":"x"}]"#],
        )],
    );

    let (report, events) = run(&ws, &backend, &reviewer, SwarmConfig::default());

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SwarmEvent::ReviewStarted { .. })),
        "no review event without the flag"
    );
    assert!(report.findings.is_empty());
    assert_eq!(report.blocking_findings, 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_diff_below_the_size_threshold_emits_no_review_events_at_all() {
    // "We didn't look" and "we looked and found nothing" are different answers,
    // and the event stream has to be able to tell them apart. Emitting
    // ReviewStarted + ReviewFinished{findings: 0} for a skipped review would read
    // as a clean four-lens review of a diff nobody examined — the same dishonesty
    // as folding Unknown into Pass.
    let (ws, merged) = duplicating_repo("threshold");
    let backend = coder(&merged);
    let reviewer = Scripted::new(
        "advisor",
        vec![(
            "QUESTION:",
            vec![r#"[{"hunk":"H0","file":"src/report/render.rs","summary":"x"}]"#],
        )],
    );

    let (report, events) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            review: ReviewConfig {
                enabled: true,
                // Far larger than this diff.
                min_changed_lines: 500,
                ..review_cfg(ReviewAction::Report)
            },
            ..Default::default()
        },
    );

    assert!(
        !events.iter().any(|e| matches!(
            e,
            SwarmEvent::ReviewStarted { .. }
                | SwarmEvent::ReviewFinding { .. }
                | SwarmEvent::ReviewFinished { .. }
        )),
        "a review that never ran says nothing"
    );
    assert!(report.findings.is_empty());
    assert!(report.all_done);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_review_that_ran_and_found_nothing_does_say_so() {
    // The other half of the same distinction: silence means unasked, so a review
    // that DID run must speak even when it found nothing.
    let (ws, merged) = duplicating_repo("clean");
    let backend = coder(&merged);
    // Unscripted: every lens replies `[]`.
    let reviewer = Scripted::new("advisor", vec![]);

    let (_, events) = run(
        &ws,
        &backend,
        &reviewer,
        SwarmConfig {
            review: review_cfg(ReviewAction::Report),
            ..Default::default()
        },
    );

    assert!(events
        .iter()
        .any(|e| matches!(e, SwarmEvent::ReviewStarted { .. })));
    let finished = events
        .iter()
        .find_map(|e| match e {
            SwarmEvent::ReviewFinished { findings, .. } => Some(*findings),
            _ => None,
        })
        .expect("a clean review still reports");
    assert_eq!(finished, 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_unreachable_reviewer_is_skipped_and_the_run_completes() {
    // A review that failed closed on a network error would make the whole gate
    // hostage to an API outage.
    struct Dead;
    impl ModelBackend for Dead {
        fn name(&self) -> &str {
            "offline"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8_192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
            Err(sc_proto::DcError::Backend("connection refused".into()))
        }
    }

    let (ws, merged) = duplicating_repo("offline");
    let backend = coder(&merged);
    let log: Mutex<Vec<SwarmEvent>> = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| log.lock().unwrap().push(e.clone()));
    let dead = Dead;
    let report = run_swarm_board(
        &backend,
        &backend,
        Some(&dead as &(dyn ModelBackend + Sync)),
        board(),
        &ws,
        &SwarmConfig {
            review: review_cfg(ReviewAction::Gate),
            ..Default::default()
        },
        &sink,
    );
    let events = log.into_inner().unwrap();

    // The run completed, and the skip is recorded explicitly rather than being
    // inferred from a narrower review.
    assert!(report.all_done, "an API outage does not fail the run");
    let skipped = events
        .iter()
        .find_map(|e| match e {
            SwarmEvent::ReviewFinished {
                reviewers_skipped, ..
            } => Some(reviewers_skipped.clone()),
            _ => None,
        })
        .expect("ReviewFinished was emitted anyway");
    assert_eq!(skipped, vec!["offline".to_string()]);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_reviewer_sees_the_integrated_diff_and_the_repo_map_the_worker_never_had() {
    // The premise of the whole spec, asserted directly: the worker's prompt does
    // not contain src/utils/date.rs, and the reviewer's does.
    let (ws, merged) = duplicating_repo("keyhole");
    let backend = coder(&merged);
    let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
    struct Watcher<'a>(&'a Mutex<Vec<String>>);
    impl ModelBackend for Watcher<'_> {
        fn name(&self) -> &str {
            "advisor"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 32_768,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
            self.0.lock().unwrap().push(
                req.messages
                    .iter()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            Ok(GenerateResponse {
                content: "[]".into(),
            })
        }
    }

    let watcher = Watcher(&seen);
    let log: Mutex<Vec<SwarmEvent>> = Mutex::new(Vec::new());
    let sink = FnSwarmSink(|e: &SwarmEvent| log.lock().unwrap().push(e.clone()));
    let _ = run_swarm_board(
        &backend,
        &backend,
        Some(&watcher as &(dyn ModelBackend + Sync)),
        board(),
        &ws,
        &SwarmConfig {
            review: review_cfg(ReviewAction::Report),
            ..Default::default()
        },
        &sink,
    );

    let prompts = seen.lock().unwrap();
    assert_eq!(prompts.len(), 4, "one call per lens, run in parallel");
    assert!(
        prompts.iter().all(|p| p.contains("REPOSITORY MAP")),
        "every lens gets the map"
    );
    // The duplication lens got the pre-retrieved original — the deterministic
    // lookup ran BEFORE the model call, which is the whole "retrieve first" point.
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("src/utils/date.rs:4") && p.contains("format_date")),
        "the lookup result did not reach the prompt"
    );

    // And the worker's own prompt never mentioned the file it duplicated.
    let worker_prompt = log
        .into_inner()
        .unwrap()
        .into_iter()
        .find_map(|e| match e {
            SwarmEvent::WorkerStarted { prompt, .. } => Some(prompt),
            _ => None,
        })
        .expect("WorkerStarted was emitted");
    assert!(
        !worker_prompt.contains("src/utils/date.rs"),
        "the worker's keyhole is narrower — that is why review exists"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
