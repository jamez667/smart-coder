//! Headless event printer: drive the agent against a real backend and print the
//! event stream as plain text (no TTY needed). Same events the TUI renders.
//!
//! Usage:
//!   cargo run -p dc-tui --example headless -- <base_url> <model> <workspace> <verify_cmd> <task...>

use std::path::PathBuf;

use dc_core::{run_agent_observed, AgentConfig, AgentEvent, FnSink};
use dc_model::{ModelBackend, OpenAiBackend};
use dc_tools::default_registry;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let base_url = args
        .first()
        .cloned()
        .unwrap_or_else(|| "http://localhost:11434/v1".into());
    let model = args.get(1).cloned().unwrap_or_else(|| "gemma4:e4b".into());
    let workspace = PathBuf::from(args.get(2).cloned().unwrap_or_else(|| ".".into()));
    let verify = args.get(3).cloned().filter(|s| !s.is_empty());
    // arg 4 is the optional advisor as `url@model` (or `model` to reuse the coder
    // url, or "-"/"" for none); the rest is the task.
    let advisor_spec = args.get(4).cloned().filter(|s| !s.is_empty() && s != "-");
    let task = args[5.min(args.len())..].join(" ");

    let backend = OpenAiBackend::new(base_url.clone(), model.clone());
    let advisor = advisor_spec.as_ref().map(|spec| {
        let (url, m) = match spec.split_once('@') {
            Some((u, m)) => (u.to_string(), m.to_string()),
            None => (base_url.clone(), spec.clone()),
        };
        OpenAiBackend::new(url, m)
    });
    let registry = default_registry();
    let strategy = dc_core::select_strategy(&backend.capabilities());
    // Qwen3 coders need /no_think or they return empty content.
    let suffix = if model.to_ascii_lowercase().contains("qwen3") {
        Some("/no_think".to_string())
    } else {
        None
    };
    let cfg = AgentConfig {
        verify_command: verify,
        max_steps: 15,
        system_suffix: suffix,
        ..Default::default()
    };

    let sink = FnSink(|e: &AgentEvent| println!("{}", fmt_event(e)));

    println!("== driving {} on a real model ==", task);
    let report = run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn ModelBackend),
        &registry,
        strategy.as_ref(),
        &task,
        &workspace,
        &cfg,
        &sink,
    );

    match report {
        Ok(r) => {
            println!("\n== result ==");
            println!("stop: {:?}", r.stop_reason);
            println!("steps: {}  verified: {:?}", r.steps, r.verified);
            println!(
                "tool calls: {}/{} valid",
                r.metrics.valid,
                r.metrics.total()
            );
            println!("changes: {}", r.change_summary);
        }
        Err(e) => eprintln!("error: {e}"),
    }
}

fn fmt_event(e: &AgentEvent) -> String {
    match e {
        AgentEvent::RunStarted {
            task,
            prompt_budget,
        } => {
            format!("▶ run: {task}  (budget {prompt_budget} tok)")
        }
        AgentEvent::Planned { steps } => format!("● plan: {}", steps.join(" | ")),
        AgentEvent::PlanRevised { steps } => format!("● re-plan: {}", steps.join(" | ")),
        AgentEvent::PromptAssembled {
            step,
            tokens,
            messages,
        } => {
            format!("⌖ prompt[{step}]: {} msgs, {tokens} tok", messages.len())
        }
        AgentEvent::ModelTurn {
            step,
            prompt_tokens,
            ..
        } => {
            format!("· turn {step} ({prompt_tokens} tok)")
        }
        AgentEvent::ToolCall { tool, arg } => format!("  ▸ {tool} {arg}"),
        AgentEvent::ToolResult {
            summary, is_error, ..
        } => {
            format!("    {} {summary}", if *is_error { "✗" } else { "✓" })
        }
        AgentEvent::RepairTriggered { detail } => format!("  ↻ repair: {detail}"),
        AgentEvent::Verification { green, summary, .. } => {
            format!(
                "  ⊨ verify [{}]: {summary}",
                if *green { "GREEN" } else { "RED" }
            )
        }
        AgentEvent::Stalled { trigger } => format!("  ⚠ stalled: {trigger}"),
        AgentEvent::Advice { advice, .. } => format!("  💡 {advice}"),
        AgentEvent::Stopped { reason } => format!("■ stopped: {reason:?}"),
    }
}
