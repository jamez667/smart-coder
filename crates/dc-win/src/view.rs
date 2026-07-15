//! Pure event→view mapping (no iced types), so the "what to show" logic is
//! host-testable and `app.rs` stays thin rendering glue. Mirrors the CLI's
//! `print_event` / `print_swarm_event` vocabulary (spec 06) — the same icons and
//! one-line summaries, as data the renderer lays out.

use dc_core::{AgentEvent, StopReason};
use dc_swarm::SwarmEvent;

/// One line in the live activity stream: a leading glyph, the text, and whether it's
/// an error/failure (so the renderer can colour it).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub icon: &'static str,
    pub text: String,
    pub is_error: bool,
}

impl Row {
    pub fn ok(icon: &'static str, text: impl Into<String>) -> Self {
        Self {
            icon,
            text: text.into(),
            is_error: false,
        }
    }
    fn err(icon: &'static str, text: impl Into<String>) -> Self {
        Self {
            icon,
            text: text.into(),
            is_error: true,
        }
    }
}

/// Map one [`AgentEvent`] to activity rows (most events → one row; a plan → a header
/// plus one row per step). `PromptAssembled` is intentionally dropped from the live
/// stream — it's the large verbose dump, surfaced elsewhere if ever wanted.
pub fn agent_rows(ev: &AgentEvent) -> Vec<Row> {
    use AgentEvent::*;
    match ev {
        RunStarted {
            task,
            prompt_budget,
        } => vec![Row::ok(
            "●",
            format!("run  {task}   (budget {prompt_budget} tok)"),
        )],
        Planned { steps } => plan_rows("plan", steps),
        PlanRevised { steps } => plan_rows("plan revised", steps),
        PromptAssembled { .. } => Vec::new(),
        // The live streaming increment — shown as a growing preview elsewhere, not a row.
        ContentDelta { .. } => Vec::new(),
        ModelTurn {
            step,
            prompt_tokens,
            raw,
        } => {
            // The turn header — always shown.
            let mut rows = vec![Row::ok("·", format!("turn {step}   ({prompt_tokens} tok)"))];
            // The model's own narration — what it says it's seeing / planning to do, BEFORE the
            // tool-call JSON. Surfacing it turns the stream from a bare list of tool calls into a
            // running account of the agent's thinking (the raw carries it; it was being dropped).
            if let Some(note) = narration(raw) {
                rows.push(Row::ok("💭", note));
            }
            rows
        }
        ToolCall { tool, arg } => vec![Row::ok("▸", format!("{tool}  {arg}"))],
        ToolResult {
            summary, is_error, ..
        } => {
            if *is_error {
                vec![Row::err("✗", summary.clone())]
            } else {
                vec![Row::ok("└", summary.clone())]
            }
        }
        RepairTriggered { detail } => vec![Row::ok("↻", format!("repair: {detail}"))],
        Verification { green, summary, .. } => {
            let icon = if *green { "✓" } else { "✗" };
            let text = format!("verify  {summary}");
            if *green {
                vec![Row::ok(icon, text)]
            } else {
                vec![Row::err(icon, text)]
            }
        }
        Stalled { trigger } => vec![Row::err("⚠", format!("stalled: {trigger}"))],
        Advice { trigger, advice } => vec![Row::ok("☎", format!("advisor ({trigger}): {advice}"))],
        Diagnosis { trigger, report } => {
            vec![Row::ok("🔬", format!("diagnosis ({trigger}): {report}"))]
        }
        Stopped { reason } => vec![stop_row(reason)],
    }
}

/// The model's own narration for a turn: the prose it wrote BEFORE its tool-call JSON, with
/// any `<think>…</think>` reasoning block removed and collapsed to a single tidy line. This is
/// the "what it's seeing / about to do" account that makes the stream readable. Returns `None`
/// when the turn is pure tool call with nothing to say (common), so no empty row is emitted.
///
/// Public so the chat/execute feed (`app::fix_feed_line`) can surface the same narration during
/// an iterate/execute run, not just the activity stream.
pub fn narration(raw: &str) -> Option<String> {
    // Drop a leading/embedded <think> block (Qwen3 et al.) — internal reasoning, not narration.
    let mut s = raw;
    let lower = s.to_ascii_lowercase();
    if let (Some(o), Some(c)) = (lower.find("<think>"), lower.find("</think>")) {
        if c > o {
            // Keep whatever prose came after the closed think block.
            s = &raw[c + "</think>".len()..];
        }
    }
    // Cut at the first tool-call JSON object — everything before it is the narration.
    let prose = match dc_core::text::extract_json_object(s) {
        Some(json) => {
            let cut = json.as_ptr() as usize - s.as_ptr() as usize;
            &s[..cut]
        }
        None => s, // No tool call this turn (e.g. a finish with a message) — it's all prose.
    };
    // Collapse whitespace/newlines to one line and trim leading list/fence noise.
    let one_line = prose
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '`' || c == '#' || c == '-' || c.is_whitespace())
        .to_string();
    if one_line.is_empty() {
        None
    } else {
        // Keep the stream tidy — a long paragraph gets clipped with an ellipsis.
        Some(clip(&one_line, 240))
    }
}

/// Clip `s` to at most `max` chars on a char boundary, appending `…` if it was cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Map one [`SwarmEvent`] to activity rows — the orchestrator/task-board vocabulary.
pub fn swarm_rows(ev: &SwarmEvent) -> Vec<Row> {
    use SwarmEvent::*;
    match ev {
        Decomposed { subtasks } => {
            let mut rows = vec![Row::ok(
                "●",
                format!("board  ({} subtasks)", subtasks.len()),
            )];
            for (i, s) in subtasks.iter().enumerate() {
                rows.push(Row::ok(" ", format!("{}. {s}", i + 1)));
            }
            rows
        }
        OrchestratorPrompt { fell_back, .. } => {
            if *fell_back {
                vec![Row::err(
                    "⚠",
                    "decomposition fell back to one subtask (orchestrator gave nothing usable)"
                        .to_string(),
                )]
            } else {
                // The prompt/reply themselves are shown in the dedicated panel, not the
                // flat stream — no row here.
                Vec::new()
            }
        }
        WorkerStarted { subtask, goal, .. } => {
            vec![Row::ok("▸", format!("worker [{subtask}]  {goal}"))]
        }
        WorkerFinished {
            subtask, summary, ..
        } => {
            vec![Row::ok("·", format!("[{subtask}] finished — {summary}"))]
        }
        SubtaskRetry {
            subtask,
            attempt,
            max,
            failing_tests,
        } => {
            let n = failing_tests.len();
            let s = if n == 1 { "" } else { "s" };
            vec![Row::err(
                "↻",
                format!("[{subtask}] retry {attempt}/{max} — {n} test{s} still red"),
            )]
        }
        AdvisorConsulted { subtask, advice } => {
            vec![Row::ok("⚑", format!("[{subtask}] asked senior — {advice}"))]
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
                vec![Row::ok("✓", format!("[{subtask}] integrated — {what}"))]
            } else {
                vec![Row::err("✗", format!("[{subtask}] reverted"))]
            }
        }
        SwarmDone {
            done,
            failed,
            all_done,
        } => {
            let icon = if *all_done { "✔" } else { "■" };
            let row = format!("swarm done — {done} integrated, {failed} failed");
            if *all_done {
                vec![Row::ok(icon, row)]
            } else {
                vec![Row::err(icon, row)]
            }
        }
    }
}

fn plan_rows(header: &str, steps: &[String]) -> Vec<Row> {
    let mut rows = vec![Row::ok("●", header.to_string())];
    for (i, s) in steps.iter().enumerate() {
        rows.push(Row::ok(" ", format!("{}. {s}", i + 1)));
    }
    rows
}

/// The honest stop line (spec 06): the run's final, truthful status.
fn stop_row(reason: &StopReason) -> Row {
    let text = format!("stopped — {reason:?}");
    // Only a clean finish is "ok"; every other stop reason is a non-success the UI
    // shows plainly rather than dressing up.
    match reason {
        StopReason::Finished => Row::ok("■", text),
        _ => Row::err("■", text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_error_is_flagged() {
        let rows = agent_rows(&AgentEvent::ToolResult {
            summary: "edit_file failed".to_string(),
            full: String::new(),
            is_error: true,
        });
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_error);
        assert_eq!(rows[0].icon, "✗");
    }

    #[test]
    fn planned_yields_a_header_plus_one_row_per_step() {
        let rows = agent_rows(&AgentEvent::Planned {
            steps: vec!["a".to_string(), "b".to_string()],
        });
        assert_eq!(rows.len(), 3, "header + 2 steps");
        assert!(rows[1].text.starts_with("1. "));
        assert!(rows[2].text.starts_with("2. "));
    }

    #[test]
    fn model_turn_surfaces_the_narration_before_the_tool_call() {
        let raw = "I'll add the water module and wire it into the terrain builder.\n\
                   {\"tool\":\"write_file\",\"path\":\"water.rs\",\"content\":\"x\"}";
        let rows = agent_rows(&AgentEvent::ModelTurn {
            step: 2,
            prompt_tokens: 100,
            raw: raw.to_string(),
        });
        assert_eq!(rows.len(), 2, "turn header + narration");
        assert_eq!(rows[1].icon, "💭");
        assert!(rows[1].text.contains("add the water module"));
        assert!(!rows[1].text.contains("write_file"), "tool JSON not in narration");
    }

    #[test]
    fn model_turn_narration_strips_a_think_block() {
        let raw = "<think>which file first?</think>Creating the water surface renderer.\n\
                   {\"tool\":\"create_file\",\"path\":\"water.rs\"}";
        let rows = agent_rows(&AgentEvent::ModelTurn {
            step: 1,
            prompt_tokens: 50,
            raw: raw.to_string(),
        });
        assert_eq!(rows.len(), 2);
        assert!(rows[1].text.contains("water surface renderer"));
        assert!(!rows[1].text.contains("which file first"), "reasoning hidden");
    }

    #[test]
    fn model_turn_with_no_narration_yields_only_the_header() {
        // Pure tool call, no prose → just the turn row, no empty 💭 line.
        let rows = agent_rows(&AgentEvent::ModelTurn {
            step: 3,
            prompt_tokens: 20,
            raw: "{\"tool\":\"read_file\",\"path\":\"a.rs\"}".to_string(),
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].icon, "·");
    }

    #[test]
    fn prompt_assembled_is_dropped_from_the_live_stream() {
        let rows = agent_rows(&AgentEvent::PromptAssembled {
            step: 0,
            tokens: 10,
            messages: Vec::new(),
        });
        assert!(rows.is_empty());
    }

    #[test]
    fn honest_stop_line_marks_non_finish_as_error() {
        let finished = agent_rows(&AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        assert!(!finished[0].is_error, "a clean finish is not an error");

        let budget = agent_rows(&AgentEvent::Stopped {
            reason: StopReason::BudgetExhausted,
        });
        assert!(
            budget[0].is_error,
            "budget-exhausted is shown as a non-success"
        );
    }

    #[test]
    fn swarm_retry_pluralizes_and_flags_red() {
        let one = swarm_rows(&SwarmEvent::SubtaskRetry {
            subtask: "T1".to_string(),
            attempt: 1,
            max: 2,
            failing_tests: vec!["t".to_string()],
        });
        assert!(
            one[0].text.contains("1 test still red"),
            "{:?}",
            one[0].text
        );
        assert!(one[0].is_error);

        let many = swarm_rows(&SwarmEvent::SubtaskRetry {
            subtask: "T1".to_string(),
            attempt: 2,
            max: 2,
            failing_tests: vec!["a".to_string(), "b".to_string()],
        });
        assert!(
            many[0].text.contains("2 tests still red"),
            "{:?}",
            many[0].text
        );
    }
}
