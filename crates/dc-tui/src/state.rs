//! The TUI's view model: a pure fold of the [`AgentEvent`] stream into
//! renderable state. Kept free of any terminal/ratatui types so it can be
//! unit-tested headless — the rendering layer reads from here.

use dc_core::{AgentEvent, StopReason};

/// A single line in the scrolling activity log, tagged for coloring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

/// How a log line should be rendered (drives color in the view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Info,
    ToolCall,
    Ok,
    Error,
    Advice,
    Stall,
    Stop,
}

/// One plan step plus whether it's been marked done by a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLine {
    pub text: String,
}

/// The full view model, updated event-by-event.
#[derive(Debug, Clone, Default)]
pub struct TuiState {
    pub task: String,
    pub plan: Vec<PlanLine>,
    pub log: Vec<LogLine>,
    pub step: usize,
    pub prompt_tokens: usize,
    pub prompt_budget: usize,
    pub valid_calls: usize,
    pub invalid_calls: usize,
    pub interventions: usize,
    /// Set once the run stops; `None` while it's live.
    pub stop: Option<StopReason>,
}

/// Cap on retained log lines (the view scrolls; old lines fall off).
const MAX_LOG: usize = 500;

impl TuiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the run has finished (a `Stopped` event was seen).
    pub fn is_done(&self) -> bool {
        self.stop.is_some()
    }

    /// A short status line (spec 06 — honest stop line).
    pub fn status_line(&self) -> String {
        match &self.stop {
            None => format!("running — step {} ", self.step),
            Some(StopReason::Finished) => "✔ finished".to_string(),
            Some(StopReason::BudgetExhausted) => "⏹ stopped — step budget exhausted".to_string(),
            Some(StopReason::Stalled(why)) => format!("⚠ stalled — {why}"),
            Some(StopReason::Escalated(q)) => format!("⤴ escalated — {q}"),
            Some(StopReason::Cancelled) => "⏹ cancelled".to_string(),
        }
    }

    /// Fold one event into the state.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RunStarted {
                task,
                prompt_budget,
            } => {
                self.task = task.clone();
                self.prompt_budget = *prompt_budget;
                self.push(LineKind::Info, format!("▶ task: {task}"));
            }
            AgentEvent::Planned { steps } => {
                self.plan = steps.iter().map(|s| PlanLine { text: s.clone() }).collect();
                self.push(LineKind::Info, format!("● plan ({} steps)", steps.len()));
            }
            AgentEvent::PlanRevised { steps } => {
                self.plan = steps.iter().map(|s| PlanLine { text: s.clone() }).collect();
                self.push(
                    LineKind::Info,
                    format!("● plan revised ({} steps)", steps.len()),
                );
            }
            AgentEvent::PromptAssembled {
                step,
                tokens,
                messages,
            } => {
                // The full prompt is large; in the live log show a compact marker
                // (the JSON/session log carries the verbatim text for inspection).
                self.push(
                    LineKind::Info,
                    format!(
                        "⌖ prompt[{step}]: {} msgs, {tokens} tok (see --json/log for full text)",
                        messages.len()
                    ),
                );
            }
            AgentEvent::ModelTurn {
                step,
                prompt_tokens,
                ..
            } => {
                self.step = *step;
                self.prompt_tokens = *prompt_tokens;
            }
            AgentEvent::ToolCall { tool, arg } => {
                self.valid_calls += 1;
                let label = if arg.is_empty() {
                    format!("▸ {tool}")
                } else {
                    format!("▸ {tool}  {arg}")
                };
                self.push(LineKind::ToolCall, label);
            }
            AgentEvent::ToolResult {
                summary, is_error, ..
            } => {
                let kind = if *is_error {
                    LineKind::Error
                } else {
                    LineKind::Ok
                };
                let mark = if *is_error { "└ ✗" } else { "└ ✓" };
                self.push(kind, format!("{mark} {summary}"));
            }
            AgentEvent::RepairTriggered { detail } => {
                self.invalid_calls += 1;
                self.push(LineKind::Error, format!("↻ repair: {detail}"));
            }
            AgentEvent::Verification { green, summary, .. } => {
                let kind = if *green {
                    LineKind::Ok
                } else {
                    LineKind::Error
                };
                self.push(kind, format!("⊨ verify: {summary}"));
            }
            AgentEvent::Stalled { trigger } => {
                self.push(LineKind::Stall, format!("⚠ stalled: {trigger}"));
            }
            AgentEvent::Advice { advice, .. } => {
                self.interventions += 1;
                self.push(LineKind::Advice, format!("💡 {advice}"));
            }
            AgentEvent::Diagnosis { report, .. } => {
                self.interventions += 1;
                self.push(LineKind::Advice, format!("🔬 {report}"));
            }
            AgentEvent::Stopped { reason } => {
                self.stop = Some(reason.clone());
                self.push(LineKind::Stop, format!("■ {}", self.status_line()));
            }
            // The live streaming increment isn't a discrete TUI line.
            AgentEvent::ContentDelta { .. } => {}
        }
    }

    fn push(&mut self, kind: LineKind, text: String) {
        self.log.push(LogLine { kind, text });
        if self.log.len() > MAX_LOG {
            let overflow = self.log.len() - MAX_LOG;
            self.log.drain(0..overflow);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_run() -> AgentEvent {
        AgentEvent::RunStarted {
            task: "fix the bug".into(),
            prompt_budget: 5120,
        }
    }

    #[test]
    fn run_started_sets_task_and_budget() {
        let mut s = TuiState::new();
        s.apply(&ev_run());
        assert_eq!(s.task, "fix the bug");
        assert_eq!(s.prompt_budget, 5120);
        assert!(!s.is_done());
        assert!(s.status_line().contains("running"));
    }

    #[test]
    fn tool_call_and_result_log_and_count() {
        let mut s = TuiState::new();
        s.apply(&AgentEvent::ToolCall {
            tool: "read_file".into(),
            arg: "a.rs".into(),
        });
        s.apply(&AgentEvent::ToolResult {
            summary: "read 10 lines".into(),
            full: "read 10 lines".into(),
            is_error: false,
        });
        assert_eq!(s.valid_calls, 1);
        assert_eq!(s.log.len(), 2);
        assert_eq!(s.log[0].kind, LineKind::ToolCall);
        assert!(s.log[0].text.contains("read_file"));
        assert_eq!(s.log[1].kind, LineKind::Ok);
    }

    #[test]
    fn errors_and_repairs_are_flagged() {
        let mut s = TuiState::new();
        s.apply(&AgentEvent::RepairTriggered {
            detail: "bad json".into(),
        });
        s.apply(&AgentEvent::ToolResult {
            summary: "file not found".into(),
            full: "file not found".into(),
            is_error: true,
        });
        assert_eq!(s.invalid_calls, 1);
        assert!(s.log.iter().any(|l| l.kind == LineKind::Error));
    }

    #[test]
    fn plan_and_revision_replace_the_plan() {
        let mut s = TuiState::new();
        s.apply(&AgentEvent::Planned {
            steps: vec!["a".into(), "b".into()],
        });
        assert_eq!(s.plan.len(), 2);
        s.apply(&AgentEvent::PlanRevised {
            steps: vec!["x".into()],
        });
        assert_eq!(s.plan.len(), 1);
        assert_eq!(s.plan[0].text, "x");
    }

    #[test]
    fn advice_counts_as_an_intervention() {
        let mut s = TuiState::new();
        s.apply(&AgentEvent::Advice {
            trigger: "looping".into(),
            advice: "try modulo".into(),
        });
        assert_eq!(s.interventions, 1);
        assert!(s.log[0].kind == LineKind::Advice);
    }

    #[test]
    fn stopped_records_reason_and_status() {
        let mut s = TuiState::new();
        s.apply(&AgentEvent::Stopped {
            reason: StopReason::Finished,
        });
        assert!(s.is_done());
        assert!(s.status_line().contains("finished"));

        let mut s2 = TuiState::new();
        s2.apply(&AgentEvent::Stopped {
            reason: StopReason::Stalled("looping".into()),
        });
        assert!(s2.status_line().contains("stalled"));
    }

    #[test]
    fn log_is_bounded() {
        let mut s = TuiState::new();
        for i in 0..(MAX_LOG + 50) {
            s.apply(&AgentEvent::ToolResult {
                summary: format!("line {i}"),
                full: format!("line {i}"),
                is_error: false,
            });
        }
        assert_eq!(s.log.len(), MAX_LOG);
        // The oldest lines fell off; the newest survive.
        assert!(s
            .log
            .last()
            .unwrap()
            .text
            .contains(&format!("{}", MAX_LOG + 49)));
    }
}
