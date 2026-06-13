//! The M0 agent loop: a bounded act → observe cycle (spec 03).
//!
//! One model turn = one tool call. The harness/loop owns the budget and the
//! observation feedback; the model only ever decides the next single action.
//! Malformed output is a normal, handled condition — it's fed back as an error
//! observation (the repair loop), not a crash.

use std::path::Path;

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;

use crate::tool::{execute, parse_tool_call, ToolOutcome};

/// Loop configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on model turns (spec 03 — budgets are first-class).
    pub max_steps: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { max_steps: 12 }
    }
}

/// What happened over a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    /// Whether the model called `finish` within budget.
    pub finished: bool,
    /// Model turns taken.
    pub steps: usize,
}

const SYSTEM_PROMPT: &str = "\
You are a coding agent working in a project directory. Each turn, respond with \
EXACTLY ONE JSON object and nothing else. Choose one tool:\n\
{\"tool\":\"read_file\",\"path\":\"<relative path>\"}\n\
{\"tool\":\"write_file\",\"path\":\"<relative path>\",\"content\":\"<full new file contents>\"}\n\
{\"tool\":\"finish\"}\n\
Paths are relative to the project root; you cannot escape it. Make the failing \
test pass. Do NOT modify any test files. Call finish when done.";

/// Run the agent against `instruction` in `workspace`, driving `backend`.
///
/// Returns once the model calls `finish` or the step budget is exhausted. A
/// backend error (e.g. the model being unavailable) propagates as `Err`.
pub fn run_agent(
    backend: &dyn ModelBackend,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    let mut messages = vec![
        Message::system(SYSTEM_PROMPT),
        Message::user(instruction.to_string()),
    ];

    for step in 0..cfg.max_steps {
        let req = GenerateRequest::new(messages.clone());
        let resp = backend.generate(&req)?;
        messages.push(Message::assistant(resp.content.clone()));

        let observation = match parse_tool_call(&resp.content) {
            Ok(tool) => match execute(&tool, workspace) {
                ToolOutcome::Finished => {
                    return Ok(AgentReport {
                        finished: true,
                        steps: step + 1,
                    })
                }
                ToolOutcome::Observation(o) => o,
            },
            // Repair loop: tell the model exactly what went wrong (spec 03).
            Err(e) => format!("ERROR: {e}. Reply with exactly one JSON tool object."),
        };
        messages.push(Message::user(observation));
    }

    Ok(AgentReport {
        finished: false,
        steps: cfg.max_steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;
    use dc_model::{CallbackBackend, GenerateResponse, MockBackend};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-core-agent-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tool_json(t: &Tool) -> String {
        serde_json::to_string(t).unwrap()
    }

    #[test]
    fn writes_a_file_then_finishes() {
        let ws = temp_dir("write");
        let script = [
            tool_json(&Tool::WriteFile {
                path: "out.txt".into(),
                content: "hi".into(),
            }),
            tool_json(&Tool::Finish),
        ];
        let backend = MockBackend::new(script);

        let report = run_agent(&backend, "create out.txt", &ws, &AgentConfig::default()).unwrap();
        assert_eq!(
            report,
            AgentReport {
                finished: true,
                steps: 2
            }
        );
        assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "hi");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn recovers_from_a_malformed_tool_call() {
        let ws = temp_dir("repair");
        // First turn is garbage; the loop must feed back an error and continue.
        let backend = MockBackend::new(["not json at all".to_string(), tool_json(&Tool::Finish)]);

        let report = run_agent(&backend, "do it", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.steps, 2);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn stops_at_the_step_budget() {
        let ws = temp_dir("budget");
        // A backend that never finishes: always asks to read the same file.
        let read = tool_json(&Tool::ReadFile { path: "x".into() });
        let backend = CallbackBackend::android_core(move |_req| {
            Ok(GenerateResponse {
                content: read.clone(),
            })
        });

        let cfg = AgentConfig { max_steps: 3 };
        let report = run_agent(&backend, "loop forever", &ws, &cfg).unwrap();
        assert_eq!(
            report,
            AgentReport {
                finished: false,
                steps: 3
            }
        );

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn propagates_backend_errors() {
        let ws = temp_dir("err");
        let backend = MockBackend::new(Vec::<String>::new()); // exhausts immediately
        assert!(run_agent(&backend, "x", &ws, &AgentConfig::default()).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
