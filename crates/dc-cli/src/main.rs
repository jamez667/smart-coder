//! `dumb-coder` binary — a thin I/O shell over [`dc_cli`] (spec 06, M0).
//!
//! Parses args, then either prints the `doctor` report or runs a line-oriented
//! chat REPL. All the testable logic is in the library; this file is just stdin/
//! stdout plumbing.

use std::io::{self, Write};
use std::process::ExitCode;

use dc_cli::{doctor_report, probe, usage, Cli, Command};
use dc_model::{GenerateRequest, Message, ModelBackend};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match Cli::parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{}", usage());
            return ExitCode::FAILURE;
        }
    };

    match &cli.command {
        Command::Help => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Command::Doctor => run_doctor(&cli),
        Command::Chat => run_chat(&cli),
        Command::Run { task } => run_task(&cli, task.clone()),
        Command::Serve { task } => serve_task(&cli, task.clone()),
        Command::Swarm { task } => swarm_task(&cli, task.clone()),
        Command::Plan { task, interactive } => plan_task(&cli, task.clone(), *interactive),
    }
}

/// Run the staged planning workflow (spec 09): the orchestrator (T1) plans each
/// phase, workers (T2) write the tests from the Phase-4 coverage plan, and — when a
/// `--verify` command is given — the swarm implements the work decomposition against
/// those tests until the suite is green. Plan artifacts land in `.dumb-coder/plan/`.
///
/// `interactive` toggles the human checkpoints: when set, the workflow halts at each
/// phase boundary for an approve/revise/send-back/abort decision (the macro gate of
/// spec 09); otherwise every gate is auto-approved.
fn plan_task(cli: &Cli, task: String, interactive: bool) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let orchestrator = cli.orchestrator();
    let worker = cli.backend();

    // Preflight: a dead/crashed backend would otherwise silently produce empty
    // plan artifacts mid-run. Fail fast with a clear message instead.
    if let Err(e) = dc_cli::preflight(&[("orchestrator", &orchestrator), ("worker", &worker)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let on_phase = |phase: dc_workflow::Phase, content: &str| {
        let preview: String = content.lines().take(8).collect::<Vec<_>>().join("\n");
        println!("\n=== {} ===\n{preview}\n…", phase.title());
    };

    // Autonomous by default; `--interactive`/`--gate`/`--ceremony`/`--gates` put a
    // human at the gates (spec 09). Adaptive ceremony scales *which* phases stop:
    // the resolved gate set decides which phases consult the stdin gate; the rest
    // auto-approve. The gate is fully harness-owned.
    let auto = dc_workflow::AutoApprove;
    let stdin_gate = StdinGate::new(&workspace);
    let gate_set = cli.ceremony_gates();
    let ceremony_gate = dc_workflow::CeremonyGate::new(gate_set, &stdin_gate);
    let gated = cli.plan_is_gated(interactive);
    let gate: &dyn dc_workflow::Gate = if gated { &ceremony_gate } else { &auto };
    if gated {
        let gated_phases: Vec<&str> = gate_set.phases().iter().map(|p| p.title()).collect();
        let tier = cli
            .ceremony
            .map(|c| c.label())
            .unwrap_or(if cli.gates.is_some() {
                "custom"
            } else {
                "full"
            });
        println!("ceremony: {tier} — gating {}", gated_phases.join(", "));
    }

    let outcome = match dc_workflow::run_workflow_gated(
        &orchestrator,
        &worker,
        &task,
        &workspace,
        cli.think_policy(),
        &on_phase,
        gate,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: workflow failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if outcome.aborted {
        println!("\nplan aborted at a checkpoint — approved artifacts kept in .dumb-coder/plan/");
        return ExitCode::SUCCESS;
    }

    println!(
        "\nplan complete — 6 phase artifacts in .dumb-coder/plan/\n  tests written: {}\n  subtasks for the swarm: {}",
        if outcome.test_files.is_empty() {
            "(none)".to_string()
        } else {
            outcome.test_files.join(", ")
        },
        outcome.board.len()
    );

    // Without a verify command there's nothing to drive the implementation against;
    // stop at the approved plan + frozen tests.
    let Some(_) = cli.verify_command.clone() else {
        println!("(no --verify given; stopping at the plan + tests. Add --verify to build it.)");
        return ExitCode::SUCCESS;
    };
    if outcome.board.is_empty() {
        eprintln!("warning: work decomposition produced no subtasks; nothing to implement");
        return ExitCode::FAILURE;
    }

    // Implement: run the swarm against the workflow's own board, gated by the
    // frozen tests the workers just wrote (the merge may never overwrite them).
    println!("\n=== implementing against the written tests ===");
    let advisor = cli.swarm_advisor();
    let mut swarm_cfg = cli.swarm_config();
    swarm_cfg.frozen_paths = outcome.test_files.clone();
    let sink = dc_swarm::NullSwarmSink;
    let report = dc_swarm::run_swarm_board(
        &orchestrator,
        &worker,
        Some(&advisor as &(dyn dc_model::ModelBackend + Sync)),
        outcome.board,
        &workspace,
        &swarm_cfg,
        &sink,
    );
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

/// The interactive checkpoint gate (spec 09): at each phase boundary it presents the
/// artifact and reads one of approve / revise / send-back / abort from stdin. The
/// artifact is already persisted to disk before we're consulted, so **revise** is
/// "edit the file, then press enter" — we re-read it (the runner picks up the edit).
struct StdinGate {
    workspace: std::path::PathBuf,
}

impl StdinGate {
    fn new(workspace: &std::path::Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }
}

impl dc_workflow::Gate for StdinGate {
    fn decide(
        &self,
        phase: dc_workflow::Phase,
        artifact: &dc_workflow::Artifact,
    ) -> dc_workflow::Decision {
        use dc_workflow::Decision;
        let file = dc_workflow::plan_dir(&self.workspace).join(phase.filename());
        let stdin = io::stdin();
        loop {
            println!(
                "\n⛳ Checkpoint: {} — review {}\n   {} lines. \
                 [a]pprove · [r]evise (edit the file, then enter) · [s]end-back · [x] abort",
                phase.title(),
                file.display(),
                artifact.content.lines().count(),
            );
            print!("decision ▸ ");
            if io::stdout().flush().is_err() {
                return Decision::Abort;
            }
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => return Decision::Abort, // EOF (Ctrl-D) — bail safely
                Ok(_) => {}
                Err(_) => return Decision::Abort,
            }
            match parse_decision(line.trim(), phase, &|prompt| read_line(&stdin, prompt)) {
                Some(d) => return d,
                None => {
                    eprintln!("  ? didn't understand that — try a, r, s, or x");
                    continue;
                }
            }
        }
    }
}

/// Read one trimmed line of input after printing `prompt`. Returns empty on EOF.
fn read_line(stdin: &io::Stdin, prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = stdin.read_line(&mut s);
    s.trim().to_string()
}

/// Parse a checkpoint decision keystroke into a [`dc_workflow::Decision`]. `ask` is
/// called for the follow-up prompts a decision needs (the send-back target phase and
/// its feedback note), so this stays pure and unit-testable — the I/O is injected.
/// Returns `None` for unrecognized input so the caller can re-prompt.
fn parse_decision(
    input: &str,
    current: dc_workflow::Phase,
    ask: &dyn Fn(&str) -> String,
) -> Option<dc_workflow::Decision> {
    use dc_workflow::{Decision, Phase};
    match input.to_ascii_lowercase().as_str() {
        "a" | "approve" | "" => Some(Decision::Approve),
        "r" | "revise" => Some(Decision::Revise),
        "x" | "abort" | "q" | "quit" => Some(Decision::Abort),
        "s" | "send-back" | "sendback" | "send" => {
            // Default target is the current phase (regenerate in place); the human
            // may name an earlier phase slug to bounce further back.
            let target_in = ask("  send back to which phase? (slug, blank = this phase) ▸ ");
            let target = if target_in.is_empty() {
                current
            } else {
                match Phase::from_slug(&target_in) {
                    Some(p) if p.index() <= current.index() => p,
                    Some(_) => {
                        eprintln!(
                            "  ! can only send back to this phase or earlier; using this phase"
                        );
                        current
                    }
                    None => {
                        eprintln!("  ! unknown phase {target_in:?}; using this phase");
                        current
                    }
                }
            };
            let notes = ask("  feedback for the regeneration (blank = none) ▸ ");
            Some(Decision::SendBack {
                target,
                notes: if notes.is_empty() { None } else { Some(notes) },
            })
        }
        _ => None,
    }
}

/// Drive a task with the worker swarm and serve the swarm dashboard.
fn swarm_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Preflight the backends before serving — a crashed server otherwise looks
    // like silent worker failures on the dashboard.
    let (orchestrator, worker, advisor) = (cli.orchestrator(), cli.backend(), cli.swarm_advisor());
    if let Err(e) = dc_cli::preflight(&[
        ("orchestrator", &orchestrator),
        ("worker", &worker),
        ("advisor", &advisor),
    ]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // Workers use --base-url/--model; the orchestrator decomposes; advisor is the
    // optional senior. All three are OpenAI-compatible backends (spec 02/08).
    let spec = dc_web::WebSwarm {
        orchestrator: cli.orchestrator(),
        worker: cli.backend(),
        // Workers always get a senior to ask: the explicit --advisor if given,
        // else the orchestrator (already in VRAM). A stalled tiny worker that can
        // ask the bigger model how to proceed is the whole recovery story (spec 02).
        advisor: Some(cli.swarm_advisor()),
        task,
        repo_overview: String::new(),
        workspace,
        config: cli.swarm_config(),
    };

    let result = dc_web::serve_swarm(spec, "127.0.0.1:0", |url| {
        println!("dumb-coder swarm dashboard live at {url}");
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

/// Drive a task in the current directory and serve a live web dashboard.
fn serve_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = dc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());

    let spec = dc_web::WebRun {
        backend,
        advisor: cli.advisor(),
        registry,
        strategy,
        instruction: task,
        workspace,
        config: cli.agent_config(),
    };

    // Bind a localhost port (0 = OS-assigned) and print the URL to open.
    let result = dc_web::serve(spec, "127.0.0.1:0", |url| {
        println!("dumb-coder dashboard live at {url}");
        println!("open it in your browser to watch the run (Ctrl-C to stop)");
    });

    match result {
        Ok(Some(report)) => {
            println!("\n{:?} — {}", report.stop_reason, report.change_summary);
            if report.finished {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: web server failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Drive a coding task in the current directory with the live TUI.
fn run_task(cli: &Cli, task: String) -> ExitCode {
    let workspace = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let backend = cli.backend();
    if let Err(e) = dc_cli::preflight(&[("model", &backend)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let registry = dc_tools::default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());

    let spec = dc_tui::TuiRun {
        backend,
        // "Junior asks senior" (spec 02): the optional larger advisor model.
        advisor: cli.advisor(),
        registry,
        strategy,
        instruction: task,
        workspace,
        config: cli.agent_config(),
    };

    match dc_tui::run(spec) {
        Ok(Some(report)) => {
            // Honest stop line on the normal terminal after the TUI restores it.
            println!("{:?} — {}", report.stop_reason, report.change_summary);
            if report.finished {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: TUI failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_doctor(cli: &Cli) -> ExitCode {
    let backend = cli.backend();
    let reachable = probe(&backend);
    let ok = reachable.is_ok();
    println!(
        "{}",
        doctor_report(cli, &backend.capabilities(), &reachable)
    );
    if ok {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "\nThe backend isn't serving the model. Is the server running and the \
             model pulled?\n  e.g.  ollama serve   &&   ollama pull {}",
            cli.model
        );
        ExitCode::FAILURE
    }
}

/// A trivial multi-turn chat REPL: read a line, generate, print, repeat. History
/// is carried so follow-ups have context (spec 06). No tools — that's M1+.
fn run_chat(cli: &Cli) -> ExitCode {
    let backend = cli.backend();
    println!(
        "dumb-coder chat — {} via {} (Ctrl-D or `exit` to quit)\n",
        cli.model, cli.base_url
    );

    let mut history = vec![Message::system(
        "You are dumb-coder, a concise terminal coding assistant.",
    )];
    let stdin = io::stdin();

    loop {
        print!("you ▸ ");
        if io::stdout().flush().is_err() {
            return ExitCode::FAILURE;
        }

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                return ExitCode::SUCCESS; // EOF (Ctrl-D)
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("input error: {e}");
                return ExitCode::FAILURE;
            }
        }

        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "exit" | "quit") {
            return ExitCode::SUCCESS;
        }

        history.push(Message::user(prompt.to_string()));
        let req = GenerateRequest::new(history.clone());
        match backend.generate(&req) {
            Ok(resp) => {
                println!("dc  ▸ {}\n", resp.content.trim());
                history.push(Message::assistant(resp.content));
            }
            Err(e) => {
                // Don't poison the history with a failed turn; let the user retry.
                history.pop();
                eprintln!("error: {e}\n");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_workflow::{Decision, Phase};

    /// An `ask` stub that hands back a fixed sequence of answers for the follow-up
    /// prompts (send-back target, then feedback note).
    fn answers(seq: &[&str]) -> impl Fn(&str) -> String {
        let seq: Vec<String> = seq.iter().map(|s| s.to_string()).collect();
        let i = std::cell::Cell::new(0);
        move |_prompt: &str| {
            let n = i.get();
            i.set(n + 1);
            seq.get(n).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn approve_revise_abort_keystrokes() {
        let no_ask = |_: &str| String::new();
        // Approve: explicit, long form, and the empty default all approve.
        for k in ["a", "approve", ""] {
            assert_eq!(
                parse_decision(k, Phase::Specs, &no_ask),
                Some(Decision::Approve)
            );
        }
        assert_eq!(
            parse_decision("r", Phase::Specs, &no_ask),
            Some(Decision::Revise)
        );
        for k in ["x", "abort", "q"] {
            assert_eq!(
                parse_decision(k, Phase::Specs, &no_ask),
                Some(Decision::Abort)
            );
        }
        // Garbage re-prompts (None).
        assert_eq!(parse_decision("huh", Phase::Specs, &no_ask), None);
    }

    #[test]
    fn send_back_defaults_to_current_phase_with_no_notes() {
        // Blank target → this phase; blank notes → None.
        let ask = answers(&["", ""]);
        assert_eq!(
            parse_decision("s", Phase::Layout, &ask),
            Some(Decision::SendBack {
                target: Phase::Layout,
                notes: None,
            })
        );
    }

    #[test]
    fn send_back_targets_an_earlier_phase_with_notes() {
        let ask = answers(&["architecture", "make it event-driven"]);
        assert_eq!(
            parse_decision("s", Phase::Layout, &ask),
            Some(Decision::SendBack {
                target: Phase::Architecture,
                notes: Some("make it event-driven".to_string()),
            })
        );
    }

    #[test]
    fn send_back_to_a_later_phase_is_clamped_to_current() {
        // You can't bounce forward — naming a downstream phase falls back to here.
        let ask = answers(&["work-decomposition", ""]);
        assert_eq!(
            parse_decision("s", Phase::Layout, &ask),
            Some(Decision::SendBack {
                target: Phase::Layout,
                notes: None,
            })
        );
    }

    #[test]
    fn send_back_unknown_phase_falls_back_to_current() {
        let ask = answers(&["nonsense", ""]);
        assert_eq!(
            parse_decision("s", Phase::Architecture, &ask),
            Some(Decision::SendBack {
                target: Phase::Architecture,
                notes: None,
            })
        );
    }
}
