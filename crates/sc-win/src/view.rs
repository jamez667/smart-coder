//! Pure event→view mapping (no iced types), so the "what to show" logic is
//! host-testable and `app.rs` stays thin rendering glue. Mirrors the CLI's
//! `print_event` / `print_swarm_event` vocabulary (spec 06) — the same icons and
//! one-line summaries, as data the renderer lays out.

use sc_core::{AgentEvent, StopReason};
use sc_swarm::SwarmEvent;

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
    /// An error/failure row — rendered in the bad colour. Public because the Claude panel
    /// builds its own closing row (spec 22) rather than going through `agent_rows`.
    pub fn err(icon: &'static str, text: impl Into<String>) -> Self {
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
        // Flagged as an error row because it is one -- ours. The wrench distinguishes
        // it at a glance from the model's own failures above.
        HarnessFault { kind, detail, .. } => vec![Row::err(
            "🔧",
            format!("harness fault ({}): {detail}", kind.label()),
        )],
        Stalled { trigger } => vec![Row::err("⚠", format!("stalled: {trigger}"))],
        Advice { trigger, advice } => vec![Row::ok("☎", format!("advisor ({trigger}): {advice}"))],
        Diagnosis { trigger, report } => {
            vec![Row::ok("🔬", format!("diagnosis ({trigger}): {report}"))]
        }
        Stopped { reason } => vec![stop_row(reason)],
        // Remote approve/deny prompts drive the web/phone client; the desktop GUI has
        // its own local confirmer, so these aren't rendered as rows here.
        ConfirmPending { .. } | ConfirmResolved { .. } => Vec::new(),
        // Chat mirror events drive the remote (phone) client, not the desktop's own row view.
        ChatMessage { .. } | ChatDelta { .. } => Vec::new(),
    }
}

/// The model's own narration for a turn: the prose it wrote BEFORE its tool-call JSON, with
/// any `<think>…</think>` reasoning block removed and collapsed to a single tidy line. This is
/// the "what it's seeing / about to do" account that makes the stream readable. Returns `None`
/// when the turn is pure tool call with nothing to say (common), so no empty row is emitted.
///
/// Public so the chat/execute feed (`app::fix_feed_line`) can surface the same narration during
/// an iterate/execute run, not just the activity stream.
/// Remove every `<think>…</think>` block, including an unterminated trailing one.
///
/// A reasoning model's output arrives as one tagged block PER STREAMED DELTA, so any
/// single-block strip leaves the rest visible. An unterminated block means the model ran
/// out of budget mid-thought: everything after it is dropped, because a half-sentence of
/// reasoning is not narration.
pub fn strip_think_blocks(raw: &str) -> String {
    let mut out = raw.to_string();
    loop {
        let lower = out.to_ascii_lowercase();
        let Some(open) = lower.find("<think>") else {
            break;
        };
        let after = match lower[open..].find("</think>") {
            Some(rel) => out[open + rel + "</think>".len()..].to_string(),
            None => String::new(),
        };
        out = format!("{}{}", out[..open].trim_end(), after);
    }
    out
}

pub fn narration(raw: &str) -> Option<String> {
    // Drop EVERY <think> block, not just the first.
    //
    // This took the first `<think>`…`</think>` pair and kept the rest, which is correct
    // for a model that emits one block. A reasoning model STREAMS its thinking, and the
    // backend tags each delta separately, so a turn arrives as
    // `<think> task </think><think> is </think><think> clear </think>…` — dozens of
    // pairs. Cutting at the first one left all the others in the feed, which is exactly
    // what the user saw:
    //
    // ```text
    // 💬 <think> task </think><think> is </think><think> clear </think>…
    // ```
    //
    // `strip_think` already loops; this now uses it rather than keeping a second,
    // subtly-different implementation of the same idea.
    let stripped = strip_think_blocks(raw);
    let s = stripped.as_str();
    // Cut at the first tool-call JSON object — everything before it is the narration.
    let prose = match sc_core::text::extract_json_object(s) {
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
        ReviewStarted {
            subtask,
            lenses,
            reviewers,
        } => {
            // Cost is lenses × reviewers, so both are named up front.
            vec![Row::ok(
                "◇",
                format!(
                    "[{subtask}] reviewing — {} lenses × {} reviewer{}",
                    lenses.len(),
                    reviewers.len(),
                    if reviewers.len() == 1 { "" } else { "s" }
                ),
            )]
        }
        ReviewFinding {
            subtask,
            lens,
            severity,
            anchor,
            corroborated,
            summary,
            raised_by,
            considered_by,
            ..
        } => {
            // The distinction the whole spec turns on, made visible: a checked
            // finding can act, an opinion cannot. Never flattened into one look.
            let mark = if *corroborated { "checked" } else { "opinion" };
            let mut where_ = anchor.file.clone();
            if let Some(sym) = &anchor.symbol {
                where_.push_str(&format!(" · {sym}"));
            }
            if let Some(line) = anchor.line {
                where_.push_str(&format!(":{line}"));
            }
            // A lone finding others reviewed and did not raise is contested — worth
            // showing as such rather than as agreement-of-one.
            let contested = considered_by.len() > 1 && raised_by.len() == 1;
            let votes = if contested {
                format!(" · contested (1 of {})", considered_by.len())
            } else if raised_by.len() > 1 {
                format!(" · {} reviewers agree", raised_by.len())
            } else {
                String::new()
            };
            let text =
                format!("[{subtask}] {lens}/{severity} ({mark}){votes} — {where_}: {summary}");
            // Only a corroborated finding is flagged as something to act on.
            if *corroborated {
                vec![Row::err("⚠", text)]
            } else {
                vec![Row::ok("·", text)]
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
                format!(" ({} reviewer(s) unreachable)", reviewers_skipped.len())
            };
            let text = if *findings == 0 {
                format!("[{subtask}] review clean{skipped}")
            } else {
                format!("[{subtask}] review — {findings} finding(s), {blocking} blocking{skipped}")
            };
            if *blocking > 0 {
                vec![Row::err("■", text)]
            } else {
                vec![Row::ok("◈", text)]
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

    /// **Per-delta think tags, from a live screenshot.**
    ///
    /// `narration` cut at the FIRST `</think>` and kept the rest, which is right for a
    /// model that emits one block. A reasoning model tags every streamed delta, so a
    /// turn arrives as dozens of pairs and all but the first survived into the activity
    /// feed.
    #[test]
    fn every_think_block_is_dropped_not_just_the_first() {
        let raw = "<think> task</think><think> is</think><think> clear</think>                   <think>.</think><think> I</think><think> need</think>                   <think> to</think><think> edit</think>";
        assert!(
            strip_think_blocks(raw).trim().is_empty(),
            "{:?}",
            strip_think_blocks(raw)
        );
        assert_eq!(narration(raw), None);
    }

    /// Real narration after the reasoning still reaches the feed -- dropping it was the
    /// older bug this function exists to fix.
    #[test]
    fn narration_after_the_thinking_survives() {
        let raw = "<think>hmm</think><think> ok</think>Reading the trail code now.";
        assert_eq!(
            narration(raw).as_deref(),
            Some("Reading the trail code now.")
        );
    }

    /// An unterminated block is a model that ran out of budget mid-thought: everything
    /// after it goes, rather than showing half a sentence of reasoning.
    #[test]
    fn an_unterminated_think_block_yields_no_narration() {
        assert_eq!(narration("<think>Wait, actually the head is"), None);
    }

    /// Prose that merely mentions thinking is not a tag.
    #[test]
    fn ordinary_prose_is_untouched() {
        let raw = "I think the fix is in starfield.rs.";
        assert_eq!(strip_think_blocks(raw), raw);
    }
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
        assert!(
            !rows[1].text.contains("write_file"),
            "tool JSON not in narration"
        );
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
        assert!(
            !rows[1].text.contains("which file first"),
            "reasoning hidden"
        );
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
