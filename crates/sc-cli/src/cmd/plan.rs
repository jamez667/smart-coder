//! `plan <task>` — the staged planning workflow, and the interactive checkpoint gate.

use std::io::{self, Write};
use std::process::ExitCode;

use sc_cli::Cli;

use super::common::workspace;

/// Run the staged planning workflow (spec 09): the orchestrator (T1) plans each
/// phase, workers (T2) write the tests from the Phase-4 coverage plan, and — when a
/// `--verify` command is given — the swarm implements the work decomposition against
/// those tests until the suite is green. Plan artifacts land in `.smart-coder/plan/`.
///
/// `interactive` toggles the human checkpoints: when set, the workflow halts at each
/// phase boundary for an approve/revise/send-back/abort decision (the macro gate of
/// spec 09); otherwise every gate is auto-approved.
pub fn plan_task(cli: &Cli, task: String, interactive: bool) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };

    let orchestrator = cli.orchestrator();
    let worker = cli.backend();

    // Preflight: a dead/crashed backend would otherwise silently produce empty
    // plan artifacts mid-run. Fail fast with a clear message instead.
    if let Err(e) = sc_cli::preflight(&[("orchestrator", &orchestrator), ("worker", &worker)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let on_phase = |phase: sc_workflow::Phase, content: &str| {
        let preview: String = content.lines().take(8).collect::<Vec<_>>().join("\n");
        println!("\n=== {} ===\n{preview}\n…", phase.title());
    };

    // Autonomous by default; `--interactive`/`--gate`/`--ceremony`/`--gates` put a
    // human at the gates (spec 09). Adaptive ceremony scales *which* phases stop:
    // the resolved gate set decides which phases consult the stdin gate; the rest
    // auto-approve. The gate is fully harness-owned.
    let auto = sc_workflow::AutoApprove;
    let stdin_gate = StdinGate::new(&workspace);
    let gate_set = cli.ceremony_gates();
    let ceremony_gate = sc_workflow::CeremonyGate::new(gate_set, &stdin_gate);
    let gated = cli.plan_is_gated(interactive);
    let gate: &dyn sc_workflow::Gate = if gated { &ceremony_gate } else { &auto };
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

    let outcome = match sc_workflow::run_workflow_gated(
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
        println!("\nplan aborted at a checkpoint — approved artifacts kept in .smart-coder/plan/");
        return ExitCode::SUCCESS;
    }

    println!(
        "\nplan complete — 6 phase artifacts in .smart-coder/plan/\n  tests written: {}\n  subtasks for the swarm: {}",
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
    let sink = sc_swarm::NullSwarmSink;
    let report = sc_swarm::run_swarm_board(
        &orchestrator,
        &worker,
        Some(&advisor as &(dyn sc_model::ModelBackend + Sync)),
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

impl sc_workflow::Gate for StdinGate {
    fn decide(
        &self,
        phase: sc_workflow::Phase,
        artifact: &sc_workflow::Artifact,
    ) -> sc_workflow::Decision {
        use sc_workflow::Decision;
        let file = sc_workflow::plan_dir(&self.workspace).join(phase.filename());
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

/// Parse a checkpoint decision keystroke into a [`sc_workflow::Decision`]. `ask` is
/// called for the follow-up prompts a decision needs (the send-back target phase and
/// its feedback note), so this stays pure and unit-testable — the I/O is injected.
/// Returns `None` for unrecognized input so the caller can re-prompt.
fn parse_decision(
    input: &str,
    current: sc_workflow::Phase,
    ask: &dyn Fn(&str) -> String,
) -> Option<sc_workflow::Decision> {
    use sc_workflow::{Decision, Phase};
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

#[cfg(test)]
mod tests {
    use super::parse_decision;
    use sc_workflow::{Decision, Phase};

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
