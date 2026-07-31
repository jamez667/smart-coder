//! `swarm <task>` — the worker swarm, and its two terminal renderers (NDJSON and
//! the human task-board view). The default path serves the web dashboard instead.

use std::process::ExitCode;

use sc_cli::Cli;

use super::common::workspace;

/// Drive a task with the worker swarm. By default this serves the live web
/// dashboard; `--cli` renders the swarm to the terminal (line-oriented), and
/// `--json` emits the `SwarmEvent` stream as NDJSON on stdout (spec 06 — "swarm
/// rendering"). `--json` implies the terminal path (the dashboard isn't headless).
pub fn swarm_task(cli: &Cli, task: String) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    // Preflight the backends before running — a crashed server otherwise looks
    // like silent worker failures on the dashboard / in the stream.
    let (orchestrator, worker, advisor) = (cli.orchestrator(), cli.backend(), cli.swarm_advisor());
    if let Err(e) = sc_cli::preflight(&[
        ("orchestrator", &orchestrator),
        ("worker", &worker),
        ("advisor", &advisor),
    ]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // `--cli` / `--json` drive the swarm directly and render its event stream to
    // the terminal, mirroring `run`'s TUI-vs-`run --json` split. The default
    // (neither flag) keeps the web dashboard.
    if cli.cli || cli.json {
        return swarm_task_cli(cli, task, &orchestrator, &worker, &advisor, &workspace);
    }

    // Workers use --base-url/--model; the orchestrator decomposes; advisor is the
    // optional senior. All three are OpenAI-compatible backends (spec 02/08).
    let spec = sc_web::WebSwarm {
        orchestrator: cli.orchestrator(),
        worker: cli.backend(),
        // Workers always get a senior to ask: the explicit --advisor if given,
        // else the orchestrator (already in VRAM). A stalled tiny worker that can
        // ask the bigger model how to proceed is the whole recovery story (spec 02).
        advisor: Some(cli.swarm_advisor()),
        task,
        repo_overview: String::new(),
        config: swarm_config_with_frozen(cli, &workspace),
        workspace,
    };

    let result = sc_web::serve_swarm(spec, "127.0.0.1:0", |url| {
        println!("smart-coder swarm dashboard live at {url}");
        println!("open it in your browser to watch the swarm (Ctrl-C to stop)");
    });

    match result {
        Ok(Some(report)) => {
            println!(
                "\nswarm: {} integrated, {} rejected, {} pending",
                report.done, report.failed, report.pending
            );
            if let Some(review) = review_summary(&report) {
                println!("{review}");
            }
            if report.all_done {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: swarm server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build the swarm config, filling `frozen_paths` from the workspace when the user
/// didn't pass `--frozen` (spec 08/11). Freezing the test oracle enables the precise
/// per-subtask scoped completion check and stops a worker from rewriting a test to
/// make it "pass"; without it the swarm falls back to the coarse whole-suite-delta
/// check. An explicit `--frozen` list always wins.
fn swarm_config_with_frozen(cli: &Cli, workspace: &std::path::Path) -> sc_swarm::SwarmConfig {
    let mut cfg = cli.swarm_config();
    if cfg.frozen_paths.is_empty() {
        cfg.frozen_paths = sc_cli::detect_test_files(workspace);
    }
    cfg
}

/// Drive the swarm and render its event stream to the terminal — the line-oriented
/// counterpart of the web dashboard (spec 06 "swarm rendering (later)"). With
/// `--json` the stream is NDJSON on stdout (one `SwarmEvent` per line, re-parseable),
/// human notes to stderr; otherwise it's the readable task-board view (`--cli`).
fn swarm_task_cli(
    cli: &Cli,
    task: String,
    orchestrator: &sc_model::OpenAiBackend,
    worker: &sc_model::OpenAiBackend,
    advisor: &sc_model::OpenAiBackend,
    workspace: &std::path::Path,
) -> ExitCode {
    let cfg = swarm_config_with_frozen(cli, workspace);
    if cli.json {
        eprintln!("swarm: {task}");
    } else {
        println!("● swarm  {task}   (max {} workers)", cfg.max_workers);
    }

    // Who decides at a review checkpoint. `--json` is a machine surface with no one
    // at the keyboard, so it stays headless: findings are reported loudly in the
    // stream and the run completes (spec 16 — never dropped, never hung on).
    let auto = sc_swarm::AutoContinue;
    let interactive = StdinReviewGate;
    let gate: &dyn sc_swarm::ReviewGate = if cli.json { &auto } else { &interactive };

    // The sink renders each orchestrator event as it happens: JSON lines for
    // machines, the task-board view for humans.
    let report = if cli.json {
        let sink = JsonSwarmSink;
        run_gated(
            orchestrator,
            worker,
            advisor,
            &task,
            workspace,
            &cfg,
            &sink,
            gate,
        )
    } else {
        let sink = sc_swarm::FnSwarmSink(|e: &sc_swarm::SwarmEvent| print_swarm_event(e));
        run_gated(
            orchestrator,
            worker,
            advisor,
            &task,
            workspace,
            &cfg,
            &sink,
            gate,
        )
    };

    // Honest closing line (spec 06): the human-readable summary goes to stderr in
    // `--json` mode so it never pollutes the NDJSON a consumer is parsing.
    let mut summary = format!(
        "swarm: {} integrated, {} rejected, {} pending",
        report.done, report.failed, report.pending
    );
    // Unresolved findings ride the closing line rather than being dropped —
    // especially headless, where no human is available to gate.
    if let Some(review) = review_summary(&report) {
        summary.push('\n');
        summary.push_str(&review);
    }
    if cli.json {
        eprintln!("{summary}");
    } else {
        println!("\n{summary}");
    }

    if report.all_done {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the swarm with a review checkpoint attached — the two sink flavours differ
/// only in their renderer, so the call is factored out rather than duplicated.
#[allow(clippy::too_many_arguments)]
fn run_gated(
    orchestrator: &sc_model::OpenAiBackend,
    worker: &sc_model::OpenAiBackend,
    advisor: &sc_model::OpenAiBackend,
    task: &str,
    workspace: &std::path::Path,
    cfg: &sc_swarm::SwarmConfig,
    sink: &dyn sc_swarm::SwarmSink,
    gate: &dyn sc_swarm::ReviewGate,
) -> sc_swarm::SwarmReport {
    sc_swarm::run_swarm_gated(
        orchestrator,
        worker,
        Some(advisor as &(dyn sc_model::ModelBackend + Sync)),
        task,
        "",
        workspace,
        cfg,
        sink,
        gate,
    )
}

/// The human checkpoint for a gating review finding (spec 16 — "Gate"), mirroring
/// the staged workflow's stdin gate.
///
/// Only reachable with `--review-action gate`, and only for a **corroborated**
/// finding at or above the gating severity — the swarm never asks a human about a
/// model's unconfirmed opinion, because a gate that cries wolf is a gate that gets
/// switched off.
struct StdinReviewGate;

impl sc_swarm::ReviewGate for StdinReviewGate {
    fn checkpoint(
        &self,
        subtask: &str,
        findings: &[sc_swarm::Finding],
        blocking: usize,
    ) -> sc_swarm::Checkpoint {
        use std::io::{self, BufRead, Write};

        println!(
            "\n⛳ Review checkpoint [{subtask}] — {blocking} confirmed finding(s) to look at:"
        );
        for f in findings.iter().filter(|f| f.corroborated) {
            println!("   · {} — {}", f.lens, f.anchor.file);
            if let Some(ev) = &f.evidence {
                println!("     {ev}");
            }
        }
        print!("continue the run? [y]es · [n]o, stop here ▸ ");
        if io::stdout().flush().is_err() {
            return sc_swarm::Checkpoint::Stop;
        }
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            // EOF (piped/headless) — the safe answer is to stop rather than to
            // barrel on past a finding nobody saw.
            Ok(0) | Err(_) => sc_swarm::Checkpoint::Stop,
            Ok(_) => match line.trim().to_ascii_lowercase().as_str() {
                "n" | "no" | "s" | "stop" => sc_swarm::Checkpoint::Stop,
                _ => sc_swarm::Checkpoint::Continue,
            },
        }
    }
}

/// A [`sc_swarm::SwarmSink`] that emits each event as one NDJSON line on stdout —
/// the swarm counterpart of [`sc_core::JsonLinesSink`], so `swarm --json` is
/// scriptable and the stream round-trips (`SwarmEvent` is Serialize↔Deserialize).
struct JsonSwarmSink;
impl sc_swarm::SwarmSink for JsonSwarmSink {
    fn record(&self, event: &sc_swarm::SwarmEvent) {
        match swarm_event_json(event) {
            Ok(line) => println!("{line}"),
            // Never let a serialization hiccup abort the swarm; note it on stderr.
            Err(e) => eprintln!("warning: could not serialize swarm event: {e}"),
        }
    }
}

/// Serialize one swarm event to a single NDJSON line (the body of [`JsonSwarmSink`],
/// split out so it's unit-testable without capturing stdout).
fn swarm_event_json(event: &sc_swarm::SwarmEvent) -> serde_json::Result<String> {
    serde_json::to_string(event)
}

/// Print one swarm event in the line-oriented style of spec 06 (mirrors
/// `print_event` for the per-worker stream): decomposition → which worker is on
/// which subtask → each integration accept/reject → the final tally.
fn print_swarm_event(ev: &sc_swarm::SwarmEvent) {
    use sc_swarm::SwarmEvent::*;
    match ev {
        Decomposed { subtasks } => {
            println!("● board  ({} subtasks)", subtasks.len());
            for (i, goal) in subtasks.iter().enumerate() {
                println!("  {}. {goal}", i + 1);
            }
        }
        OrchestratorPrompt { fell_back, .. } => {
            if *fell_back {
                println!(
                    "  ⚠ decomposition fell back to one subtask (orchestrator gave nothing usable)"
                );
            }
        }
        WorkerStarted { subtask, goal, .. } => {
            println!("▸ worker [{subtask}]  {goal}");
        }
        WorkerFinished {
            subtask, summary, ..
        } => {
            println!("  · [{subtask}] finished — {summary}");
        }
        SubtaskRetry {
            subtask,
            attempt,
            max,
            failing_tests,
        } => {
            let n = failing_tests.len();
            let plural = if n == 1 { "" } else { "s" };
            println!("  ↻ [{subtask}] retry {attempt}/{max} — {n} test{plural} still red");
        }
        AdvisorConsulted { subtask, advice } => {
            println!("  ⚑ [{subtask}] asked senior — {advice}");
        }
        Integrated {
            subtask,
            accepted,
            files,
        } => {
            if *accepted {
                let what = if files.is_empty() {
                    "(no file changes)".to_string()
                } else {
                    files.join(", ")
                };
                println!("  ✓ [{subtask}] integrated — {what}");
            } else {
                // On reject, `files[0]` carries the reason (spec / event.rs).
                let reason = files.first().map(String::as_str).unwrap_or("rejected");
                println!("  ✗ [{subtask}] reverted — {reason}");
            }
        }
        ReviewStarted {
            subtask,
            lenses,
            reviewers,
        } => {
            // Cost is lenses × reviewers, named before the calls rather than after.
            println!(
                "  ◇ [{subtask}] reviewing — {} lenses × {} reviewer{}",
                lenses.len(),
                reviewers.len(),
                if reviewers.len() == 1 { "" } else { "s" }
            );
        }
        ReviewFinding {
            subtask,
            lens,
            severity,
            anchor,
            corroborated,
            evidence,
            raised_by,
            considered_by,
            summary,
        } => {
            // The asymmetry made visible. A checked finding is marked and may act;
            // an opinion is shown plainly and never can. Never flattened into one.
            let mark = if *corroborated { "⚠" } else { "·" };
            let kind = if *corroborated { "checked" } else { "opinion" };
            let mut place = anchor.file.clone();
            if let Some(sym) = &anchor.symbol {
                place.push_str(&format!(" · {sym}"));
            }
            if let Some(line) = anchor.line {
                place.push_str(&format!(":{line}"));
            }
            // A lone finding others reviewed and did not raise is contested — a
            // different thing from one nobody else looked at.
            let votes = if considered_by.len() > 1 && raised_by.len() == 1 {
                format!(" · contested (1 of {})", considered_by.len())
            } else if raised_by.len() > 1 {
                format!(" · {} reviewers agree", raised_by.len())
            } else {
                String::new()
            };
            println!("  {mark} [{subtask}] {lens}/{severity} ({kind}){votes} — {place}");
            println!("      {summary}");
            // The evidence is what a worker would be handed; showing it lets a human
            // judge the finding on the same basis a retry would act on it.
            if let Some(ev) = evidence {
                println!("      evidence: {ev}");
            }
        }
        ReviewFinished {
            subtask,
            findings,
            blocking,
            reviewers_skipped,
        } => {
            // "3 of 4 reviewers ran" — a narrower review is never reported as a
            // complete one.
            let skipped = if reviewers_skipped.is_empty() {
                String::new()
            } else {
                format!(
                    " ({} unreachable: {})",
                    reviewers_skipped.len(),
                    reviewers_skipped.join(", ")
                )
            };
            if *findings == 0 {
                println!("  ◈ [{subtask}] review clean{skipped}");
            } else {
                println!(
                    "  ◈ [{subtask}] review — {findings} finding(s), {blocking} blocking{skipped}"
                );
            }
        }
        SwarmDone {
            done,
            failed,
            all_done,
        } => {
            let mark = if *all_done { "✔" } else { "■" };
            println!("{mark} swarm done — {done} integrated, {failed} failed");
        }
    }
}

/// The closing review line, when review ran and found something.
///
/// Findings are reported **loudly and never dropped**, including in the headless
/// case where no human is available to gate: the run reports "green, with
/// reservations" rather than flattening it to a pass (spec 16 / spec 06 — honest
/// stop). Returns `None` when there is nothing to say.
fn review_summary(report: &sc_swarm::SwarmReport) -> Option<String> {
    let total: usize = report.findings.iter().map(|(_, f)| f.len()).sum();
    if total == 0 && !report.stopped_at_checkpoint {
        return None;
    }
    let mut out = format!(
        "review: {total} unresolved finding(s) across {} subtask(s)",
        report.findings.len()
    );
    if report.blocking_findings > 0 {
        out.push_str(&format!(
            ", {} confirmed at or above the gating severity — green, with reservations",
            report.blocking_findings
        ));
    }
    // A run that stopped for a human ended for a reason, not from failure. Saying
    // so keeps the pending subtasks from reading as work that went wrong.
    if report.stopped_at_checkpoint {
        out.push_str(
            "\nreview: stopped at a checkpoint — everything integrated stayed integrated; \
             the remaining subtasks are still pending",
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{print_swarm_event, swarm_event_json};

    /// Every swarm event variant the CLI renders. Kept in sync with the renderer
    /// (`print_swarm_event`) and the JSON sink so both are exercised over the full
    /// set, not just the happy path.
    fn all_swarm_events() -> Vec<sc_swarm::SwarmEvent> {
        use sc_swarm::SwarmEvent::*;
        vec![
            Decomposed {
                subtasks: vec!["add validation".into(), "add a test".into()],
            },
            WorkerStarted {
                subtask: "s1".into(),
                goal: "add validation".into(),
                prompt: "Task: add validation\n…".into(),
            },
            WorkerFinished {
                subtask: "s1".into(),
                summary: "edited config.py".into(),
                proposal: "proposed body".into(),
            },
            SubtaskRetry {
                subtask: "s1".into(),
                attempt: 1,
                max: 2,
                failing_tests: vec!["test_upper_bound".into()],
            },
            AdvisorConsulted {
                subtask: "s1".into(),
                advice: "also clamp the upper bound".into(),
            },
            Integrated {
                subtask: "s1".into(),
                accepted: true,
                files: vec!["config.py".into()],
            },
            // Accepted with no file changes — the empty-files branch.
            Integrated {
                subtask: "s2".into(),
                accepted: true,
                files: vec![],
            },
            // Rejected — files[0] is the reason.
            Integrated {
                subtask: "s3".into(),
                accepted: false,
                files: vec!["suite went red".into()],
            },
            ReviewStarted {
                subtask: "s1".into(),
                lenses: vec!["duplication".into(), "error-handling".into()],
                reviewers: vec!["qwen".into()],
            },
            // Corroborated: carries evidence, and is marked as able to act.
            ReviewFinding {
                subtask: "s1".into(),
                lens: "duplication".into(),
                severity: "high".into(),
                anchor: sc_swarm::ReviewAnchor {
                    file: "src/report/render.rs".into(),
                    hunk: Some(0),
                    symbol: Some("format_date".into()),
                    line: Some(12),
                },
                corroborated: true,
                evidence: Some("`format_date` already exists at src/utils/date.rs:41".into()),
                raised_by: vec!["qwen".into()],
                considered_by: vec!["qwen".into()],
                summary: "reimplements the date helper".into(),
            },
            // Uncorroborated and contested — the no-evidence, no-anchor branch.
            ReviewFinding {
                subtask: "s1".into(),
                lens: "abstraction-fit".into(),
                severity: "low".into(),
                anchor: sc_swarm::ReviewAnchor {
                    file: "src/a.rs".into(),
                    hunk: None,
                    symbol: None,
                    line: None,
                },
                corroborated: false,
                evidence: None,
                raised_by: vec!["qwen".into()],
                considered_by: vec!["qwen".into(), "gemini".into()],
                summary: "doesn't match the surrounding style".into(),
            },
            ReviewFinished {
                subtask: "s1".into(),
                findings: 2,
                blocking: 1,
                reviewers_skipped: vec!["offline".into()],
            },
            // The clean case, with every reviewer reachable.
            ReviewFinished {
                subtask: "s2".into(),
                findings: 0,
                blocking: 0,
                reviewers_skipped: vec![],
            },
            SwarmDone {
                done: 2,
                failed: 1,
                all_done: false,
            },
        ]
    }

    /// A report carrying unresolved findings, as the last-retry case produces.
    fn report_with_findings(blocking: usize) -> sc_swarm::SwarmReport {
        let mut f = sc_swarm::Finding::new(
            sc_swarm::Lens::Duplication,
            sc_swarm::Severity::High,
            sc_swarm::Anchor::file("src/report/render.rs"),
            "reimplements the date helper",
            sc_swarm::ModelId::new("qwen"),
        );
        if blocking > 0 {
            f.corroborate("`format_date` already exists at src/utils/date.rs:41");
        }
        sc_swarm::SwarmReport {
            done: 1,
            failed: 0,
            pending: 0,
            all_done: true,
            integrated_files: vec!["src/report/render.rs".into()],
            findings: vec![("s1".to_string(), vec![f])],
            blocking_findings: blocking,
            stopped_at_checkpoint: false,
        }
    }

    #[test]
    fn a_headless_run_reports_its_findings_loudly_rather_than_dropping_them() {
        // The spec is explicit: where no human is available to gate, the run
        // completes and the findings are reported loudly — never dropped.
        let text = super::review_summary(&report_with_findings(1)).expect("something to say");
        assert!(text.contains("1 unresolved finding"), "{text}");
        assert!(
            text.contains("green, with reservations"),
            "an honest stop, not a flattened pass: {text}"
        );
    }

    #[test]
    fn findings_that_cannot_gate_are_still_reported_but_not_called_blocking() {
        let text = super::review_summary(&report_with_findings(0)).expect("still reported");
        assert!(text.contains("1 unresolved finding"), "{text}");
        assert!(
            !text.contains("gating severity"),
            "an opinion never reads as a blocker: {text}"
        );
    }

    #[test]
    fn a_run_with_no_findings_says_nothing_extra() {
        let clean = sc_swarm::SwarmReport {
            done: 1,
            failed: 0,
            pending: 0,
            all_done: true,
            integrated_files: vec![],
            findings: vec![],
            blocking_findings: 0,
            stopped_at_checkpoint: false,
        };
        assert!(super::review_summary(&clean).is_none());
    }

    #[test]
    fn swarm_renderer_handles_every_variant() {
        // The line renderer must not panic on any variant (incl. the empty-files
        // and rejected branches). Output goes to stdout; we only assert no panic.
        for ev in all_swarm_events() {
            print_swarm_event(&ev);
        }
    }

    #[test]
    fn json_sink_lines_round_trip() {
        // The `--json` swarm surface must emit one re-parseable NDJSON line per
        // event (parity with `run --json`): serialize, then deserialize back.
        for ev in all_swarm_events() {
            let line = swarm_event_json(&ev).expect("serialize");
            assert!(
                !line.contains('\n'),
                "NDJSON line must be single-line: {line}"
            );
            let back: sc_swarm::SwarmEvent = serde_json::from_str(&line).expect("deserialize back");
            assert_eq!(back, ev, "round-trip mismatch for {line}");
        }
    }
}
