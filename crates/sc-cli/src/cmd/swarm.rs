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

    // The sink renders each orchestrator event as it happens: JSON lines for
    // machines, the task-board view for humans.
    let report = if cli.json {
        let sink = JsonSwarmSink;
        sc_swarm::run_swarm(
            orchestrator,
            worker,
            Some(advisor as &(dyn sc_model::ModelBackend + Sync)),
            &task,
            "",
            workspace,
            &cfg,
            &sink,
        )
    } else {
        let sink = sc_swarm::FnSwarmSink(|e: &sc_swarm::SwarmEvent| print_swarm_event(e));
        sc_swarm::run_swarm(
            orchestrator,
            worker,
            Some(advisor as &(dyn sc_model::ModelBackend + Sync)),
            &task,
            "",
            workspace,
            &cfg,
            &sink,
        )
    };

    // Honest closing line (spec 06): the human-readable summary goes to stderr in
    // `--json` mode so it never pollutes the NDJSON a consumer is parsing.
    let summary = format!(
        "swarm: {} integrated, {} rejected, {} pending",
        report.done, report.failed, report.pending
    );
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
            SwarmDone {
                done: 2,
                failed: 1,
                all_done: false,
            },
        ]
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
