//! The agent loop: a bounded act → observe cycle (spec 03).
//!
//! One model turn = one tool call. The harness owns the budget and the
//! observation feedback; the model only ever decides the next single action.
//! Malformed output is a normal, handled condition — it's fed back through the
//! repair loop (spec 03), never acted on and never a crash.
//!
//! The loop is parameterized over a [`ToolRegistry`] and a [`ToolCallStrategy`]
//! (spec 04/02), so growing the tool surface or changing how tool calls are
//! decoded never touches this file.

use std::path::Path;

use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;
use dc_tools::{execute, ToolOutcome, ToolRegistry};

use crate::metrics::ToolCallMetrics;
use crate::strategy::ToolCallStrategy;

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
    /// Tool-call validity metrics over the run (spec 07 — the M1 ≥95% target).
    pub metrics: ToolCallMetrics,
}

const TASK_PREFIX: &str = "You are a coding agent working in a project directory. \
Make the failing test pass.\n\n";

/// Run the agent against `instruction` in `workspace` with the default registry,
/// choosing the strongest tool-call strategy the backend can enforce (spec 02).
pub fn run_agent(
    backend: &dyn ModelBackend,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    let registry = dc_tools::default_registry();
    let strategy = crate::strategy::select_strategy(&backend.capabilities());
    run_agent_with(
        backend,
        &registry,
        strategy.as_ref(),
        instruction,
        workspace,
        cfg,
    )
}

/// Run the agent with an explicit registry and tool-call strategy.
///
/// Returns once the model calls `finish` or the step budget is exhausted. A
/// backend error (e.g. the model being unavailable) propagates as `Err`.
pub fn run_agent_with(
    backend: &dyn ModelBackend,
    registry: &ToolRegistry,
    strategy: &dyn ToolCallStrategy,
    instruction: &str,
    workspace: &Path,
    cfg: &AgentConfig,
) -> Result<AgentReport> {
    let system = format!("{TASK_PREFIX}{}", strategy.system_preamble(registry));
    let mut messages = vec![
        Message::system(system),
        Message::user(instruction.to_string()),
    ];
    let mut metrics = ToolCallMetrics::default();

    for step in 0..cfg.max_steps {
        let mut req = GenerateRequest::new(messages.clone());
        strategy.prepare_request(&mut req, registry);
        let resp = backend.generate(&req)?;
        messages.push(Message::assistant(resp.content.clone()));

        let observation = match strategy.extract(&resp.content, registry) {
            Ok(call) => {
                metrics.record_valid();
                match execute(&call, workspace) {
                    ToolOutcome::Finished => {
                        return Ok(AgentReport {
                            finished: true,
                            steps: step + 1,
                            metrics,
                        })
                    }
                    ToolOutcome::Observation(o) => o,
                }
            }
            // Repair loop: tell the model exactly what went wrong (spec 03). The
            // bad call is never executed — only fed back as a precise error.
            Err(e) => {
                metrics.record_invalid();
                e.repair_prompt()
            }
        };
        messages.push(Message::user(observation));
    }

    Ok(AgentReport {
        finished: false,
        steps: cfg.max_steps,
        metrics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::{CallbackBackend, GenerateResponse, MockBackend};
    use serde_json::json;

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

    #[test]
    fn writes_a_file_then_finishes() {
        let ws = temp_dir("write");
        let backend = MockBackend::new([
            json!({"tool":"write_file","path":"out.txt","content":"hi"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);

        let report = run_agent(&backend, "create out.txt", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.steps, 2);
        assert_eq!(report.metrics.valid, 2);
        assert_eq!(report.metrics.invalid, 0);
        assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "hi");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn recovers_from_a_malformed_tool_call() {
        let ws = temp_dir("repair");
        // First turn is garbage; the loop must feed back an error and continue.
        let backend = MockBackend::new([
            "not json at all".to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);

        let report = run_agent(&backend, "do it", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.steps, 2);
        // One invalid (the garbage), one valid (the finish).
        assert_eq!(report.metrics.invalid, 1);
        assert_eq!(report.metrics.valid, 1);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_schema_violation_is_repaired_not_executed() {
        let ws = temp_dir("schema-repair");
        // read_file without a path is valid JSON but invalid against the schema;
        // it must be fed back, not executed, then the model recovers.
        let backend = MockBackend::new([
            json!({"tool":"read_file"}).to_string(),
            json!({"tool":"finish"}).to_string(),
        ]);
        let report = run_agent(&backend, "x", &ws, &AgentConfig::default()).unwrap();
        assert!(report.finished);
        assert_eq!(report.metrics.invalid, 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn stops_at_the_step_budget() {
        let ws = temp_dir("budget");
        // A backend that never finishes: always asks to read the same file.
        let read = json!({"tool":"read_file","path":"x"}).to_string();
        let backend = CallbackBackend::android_core(move |_req| {
            Ok(GenerateResponse {
                content: read.clone(),
            })
        });

        let cfg = AgentConfig { max_steps: 3 };
        let report = run_agent(&backend, "loop forever", &ws, &cfg).unwrap();
        assert!(!report.finished);
        assert_eq!(report.steps, 3);
        assert_eq!(report.metrics.valid, 3);

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
