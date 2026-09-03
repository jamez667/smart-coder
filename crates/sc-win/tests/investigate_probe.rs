//! ONE instrumented run of the investigate path against a real project and a real model.
//!
//! Ignored by default (needs the local backend). Writes a full transcript — every
//! assembled prompt, every raw reply, every tool call and result, every harness fault —
//! to `logs/investigate-probe.md`, so the run can be read after the fact rather than
//! inferred from a screenshot.
//!
//! Run with:
//!   cargo test -p sc-win --test investigate_probe -- --ignored --nocapture

use std::fmt::Write as _;
use std::io::Write as _;

const QUESTION: &str = "Can you investigate why on the jump screen the trail behind the \
                        stars it thin before it gets thick? it should be the other way around.";

#[test]
#[ignore]
fn probe_the_star_trail_question() {
    let ws = std::path::PathBuf::from(r"C:\Users\mail\working\Personal\Games\void-claim");
    assert!(ws.join("crates").is_dir(), "void-claim not found at {ws:?}");

    let cfg = sc_win::config::UiConfig::load();
    let mut log = String::new();
    let _ = writeln!(log, "# Investigate probe\n");
    let _ = writeln!(log, "- workspace: `{}`", ws.display());
    let _ = writeln!(log, "- backend: `{}`", cfg.base_url);
    let _ = writeln!(log, "- model: `{}`", cfg.model);
    let _ = writeln!(log, "\n## Question\n\n> {QUESTION}\n");

    let (tx, rx) = std::sync::mpsc::channel();
    let cfg2 = cfg.clone();
    let ws2 = ws.clone();
    let started = std::time::Instant::now();
    let h = std::thread::spawn(move || {
        sc_win::session::agent::investigate(cfg2, QUESTION.to_string(), ws2, tx);
    });

    let mut answer = String::new();
    let mut tool_calls = 0usize;
    let mut faults = 0usize;
    let mut turns = 0usize;
    while let Ok(ev) = rx.recv() {
        match ev {
            sc_win::session::UiEvent::Agent(a) => match a {
                sc_core::AgentEvent::RunStarted {
                    task,
                    prompt_budget,
                } => {
                    let _ = writeln!(log, "## Task given to the agent\n");
                    let _ = writeln!(log, "- prompt_budget: {prompt_budget} tokens\n");
                    let _ = writeln!(log, "```\n{task}\n```\n");
                    let _ = writeln!(log, "## Transcript\n");
                }
                sc_core::AgentEvent::PromptAssembled {
                    step,
                    tokens,
                    messages,
                } => {
                    let _ = writeln!(log, "### Prompt for step {step} ({tokens} tokens)\n");
                    for m in messages {
                        let _ = writeln!(log, "**{}**\n\n```\n{}\n```\n", m.role, m.content);
                    }
                }
                sc_core::AgentEvent::ModelTurn {
                    step,
                    prompt_tokens,
                    raw,
                } => {
                    turns += 1;
                    let _ = writeln!(
                        log,
                        "### Reply at step {step} (prompt {prompt_tokens} tokens, reply {} chars)\n",
                        raw.len()
                    );
                    let _ = writeln!(log, "```\n{raw}\n```\n");
                    println!("  step {step}: reply {} chars", raw.len());
                }
                sc_core::AgentEvent::ToolCall { tool, arg } => {
                    tool_calls += 1;
                    let short: String = arg.chars().take(120).collect();
                    let _ = writeln!(log, "**TOOL CALL** `{tool}` — `{short}`\n");
                    println!("  [{tool_calls}] {tool}  {short}");
                    if tool == "finish" {
                        answer = arg;
                    }
                }
                sc_core::AgentEvent::ToolResult {
                    summary,
                    full,
                    is_error,
                } => {
                    let _ = writeln!(
                        log,
                        "**RESULT**{} ({} chars)\n\n```\n{}\n```\n",
                        if is_error { " — ERROR" } else { "" },
                        full.len(),
                        summary.chars().take(400).collect::<String>()
                    );
                    if is_error {
                        println!(
                            "      !! ERROR: {}",
                            summary.chars().take(120).collect::<String>()
                        );
                    }
                }
                sc_core::AgentEvent::RepairTriggered { detail } => {
                    let _ = writeln!(log, "**REPAIR** {detail}\n");
                    println!(
                        "      ~~ REPAIR: {}",
                        detail.chars().take(120).collect::<String>()
                    );
                }
                sc_core::AgentEvent::HarnessFault { kind, detail, step } => {
                    faults += 1;
                    let _ = writeln!(
                        log,
                        "**HARNESS FAULT** ({}) at step {step}: {detail}\n",
                        kind.label()
                    );
                    println!(
                        "      ** FAULT {}: {}",
                        kind.label(),
                        detail.chars().take(140).collect::<String>()
                    );
                }
                sc_core::AgentEvent::Stopped { reason } => {
                    let _ = writeln!(log, "**STOPPED**: {reason:?}\n");
                }
                _ => {}
            },
            sc_win::session::UiEvent::Failed(m) => {
                let _ = writeln!(log, "\n**FAILED**: {m}\n");
                println!("FAILED: {m}");
            }
            sc_win::session::UiEvent::Done { ok, summary } => {
                let _ = writeln!(log, "\n**DONE** ok={ok} — {summary}\n");
                println!("DONE ok={ok} :: {summary}");
                break;
            }
            _ => {}
        }
    }
    h.join().unwrap();
    let secs = started.elapsed().as_secs();

    let _ = writeln!(log, "\n## Answer\n");
    if answer.trim().is_empty() {
        let _ = writeln!(log, "_none — the loop never called `finish`._\n");
    } else {
        let _ = writeln!(log, "{answer}\n");
    }
    let _ = writeln!(
        log,
        "\n## Summary\n\n- elapsed: {secs}s\n- model turns: {turns}\n- tool calls: {tool_calls}\n\
         - harness faults: {faults}\n- answered: {}\n",
        !answer.trim().is_empty()
    );

    let dir = std::path::Path::new("logs");
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("investigate-probe.md");
    let mut f = std::fs::File::create(&path).expect("write the probe log");
    f.write_all(log.as_bytes()).unwrap();
    println!(
        "\nWROTE {} ({} bytes, {}s)",
        path.display(),
        log.len(),
        secs
    );
}
