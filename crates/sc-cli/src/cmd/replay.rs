//! `replay <id>` — re-render a recorded session from its JSON-lines log, plus the
//! line-oriented event formatter that renders it (spec 06).

use std::process::ExitCode;

use super::common::workspace;

/// Re-render a recorded session (`replay <id>`, spec 06): read the JSON-lines log,
/// deserialize each event, and print it with the same line-oriented formatter used
/// live. A bare id resolves under `.smart-coder/sessions/`; a path is used directly.
pub fn replay(session: String) -> ExitCode {
    let Some(workspace) = workspace() else {
        return ExitCode::FAILURE;
    };
    let path = sc_cli::resolve_replay_path(&workspace, &session);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "error: cannot read session log {}: {e}\n  \
                 (looked for a session id under .smart-coder/sessions/ or a path)",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    println!("▶ replay {} ({})\n", session, path.display());
    let mut n = 0usize;
    let mut bad = 0usize;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<sc_core::AgentEvent>(line) {
            Ok(ev) => {
                print_event(&ev);
                n += 1;
            }
            Err(e) => {
                eprintln!("  ! line {}: not a valid event ({e})", i + 1);
                bad += 1;
            }
        }
    }
    println!(
        "\n— end of replay: {n} events{}",
        if bad > 0 {
            format!(", {bad} unreadable")
        } else {
            String::new()
        }
    );
    ExitCode::SUCCESS
}

/// Print one event in the line-oriented style of spec 06 (the static surface for
/// replay; the live TUI/web renderers consume the same events differently).
fn print_event(ev: &sc_core::AgentEvent) {
    use sc_core::AgentEvent::*;
    match ev {
        RunStarted {
            task,
            prompt_budget,
        } => {
            println!("● run  {task}   (budget {prompt_budget} tok)");
        }
        Planned { steps } => {
            println!("● plan");
            for (i, s) in steps.iter().enumerate() {
                println!("  {}. {s}", i + 1);
            }
        }
        PlanRevised { steps } => {
            println!("● plan revised");
            for (i, s) in steps.iter().enumerate() {
                println!("  {}. {s}", i + 1);
            }
        }
        PromptAssembled {
            step,
            tokens,
            messages,
        } => {
            // Verbose: the full prompt the model saw (spec 06). Print every message
            // verbatim so replay reproduces exactly what was sent.
            println!("⌖ prompt[{step}]  ({} msgs, {tokens} tok)", messages.len());
            for m in messages {
                println!("  ┌─ {} ─────────", m.role);
                for line in m.content.lines() {
                    println!("  │ {line}");
                }
            }
        }
        ModelTurn {
            step,
            prompt_tokens,
            ..
        } => {
            println!("· turn {step}   ({prompt_tokens} tok)");
        }
        ToolCall { tool, arg } => {
            println!("▸ {tool}  {arg}");
        }
        ToolResult {
            summary, is_error, ..
        } => {
            let mark = if *is_error { "✗" } else { "└" };
            println!("  {mark} {summary}");
        }
        RepairTriggered { detail } => {
            println!("  ↻ repair: {detail}");
        }
        Verification { green, summary, .. } => {
            println!("▸ verify  {} {summary}", if *green { "✓" } else { "✗" });
        }
        Stalled { trigger } => {
            println!("  ⚠ stalled: {trigger}");
        }
        Advice { trigger, advice } => {
            println!("  ☎ advisor ({trigger}): {advice}");
        }
        Diagnosis { trigger, report } => {
            println!("  🔬 diagnosis ({trigger}): {report}");
        }
        Stopped { reason } => {
            println!("■ stopped — {reason:?}");
        }
        ConfirmPending { id, command, .. } => {
            println!("  ⏸ approval needed [{id}]: {command}");
        }
        ConfirmResolved { id, allowed } => {
            println!("  {} resolved [{id}]", if *allowed { "✓" } else { "✗" });
        }
        ChatMessage { role, text } => {
            println!("  💬 {role}: {text}");
        }
        // The streaming increments aren't discrete CLI lines (the full text arrives at end).
        ContentDelta { .. } | ChatDelta { .. } => {}
    }
}
