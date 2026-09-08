//! The no-harness control arm (plan Phase 0.4): the model, native tool calling,
//! and nothing else.
//!
//! Every number the harness reports is "the model plus the harness". Without a
//! control there is no way to say which of the two a gain or a loss belongs to.
//! [`RawSolver`] is that control: one system message, an append-only transcript,
//! six tools attached natively, and the executor. It has NO repo map, plan,
//! nudges, stall detection, truncation heuristics, permission gate, history
//! compaction or repair prompt. When it does something dumb, that is the point --
//! the dumb thing is what the harness is being measured against.
//!
//! The only concession is a hard 16 KB byte cap on a single tool result, because
//! a raw `cat` of a large file would otherwise blow the context outright and the
//! run would measure the server's request limit rather than the model.

use std::cell::{Cell, RefCell};
use std::path::Path;

use sc_core::{AgentConfig, TokenCounter, ToolCallMetrics};
use sc_model::{GenerateRequest, Message, ModelBackend, OutputConstraint, ToolSchema};
use sc_proto::Result;
use sc_tools::{execute, params_json_schema, ToolOutcome, ToolRegistry, ValidatedCall};
use sc_verify::{run_command_in, run_verification_in, Sandbox};

use crate::solver::{RunInfo, Solver};
use crate::task::EvalTask;

/// Largest tool result fed back, in bytes. Beyond it the middle is cut out.
const RESULT_BYTE_CAP: usize = 16 * 1024;

/// What the model is told when its reply carried no tool call. Nothing else is
/// ever said to it unprompted.
const NO_CALL_REPLY: &str = "No tool call found. Reply with a tool call.";

/// The baseline solver: native tool calling with no harness around it.
pub struct RawSolver<'a> {
    backend: &'a dyn ModelBackend,
    cfg: AgentConfig,
    last_run: RefCell<Option<RunInfo>>,
    last: Cell<Option<ToolCallMetrics>>,
}

impl<'a> RawSolver<'a> {
    /// Only `cfg.max_steps` and `cfg.response_reserve_tokens` are consulted; every
    /// other field configures a harness this arm does not have.
    pub fn new(backend: &'a dyn ModelBackend, cfg: AgentConfig) -> Self {
        Self {
            backend,
            cfg,
            last_run: RefCell::new(None),
            last: Cell::new(None),
        }
    }
}

/// The same six tools a harness task run offers, so the two arms differ only in
/// the harness. Mirrors `solver::task_registry` (private to that module) rather
/// than sharing it: that one is pinned to what the scored runs measured and must
/// not drift because the control changed.
fn raw_registry() -> ToolRegistry {
    const KEEP: [&str; 6] = [
        "read_file",
        "edit_file",
        "write_file",
        "run_command",
        "run_verification",
        "finish",
    ];
    let registry = sc_tools::default_registry()
        .only(&KEEP)
        .expect("the default registry declares the six kept tools");
    debug_assert_eq!(
        registry.specs().len(),
        KEEP.len(),
        "a kept tool is missing by name"
    );
    registry
}

/// The tool schemas as the backend forwards them (`tools`/`tool_choice`). Same
/// construction as the `NativeTools` strategy in sc-core, not imported from it:
/// this arm must not depend on the thing it is a control for.
fn tool_schemas(registry: &ToolRegistry) -> Vec<ToolSchema> {
    registry
        .specs()
        .iter()
        .map(|s| ToolSchema {
            name: s.name.to_string(),
            description: s.description.to_string(),
            parameters: params_json_schema(s),
        })
        .collect()
}

/// The reply as a call, or nothing. The backend normalizes a native tool call to
/// the `{"tool": ..., ...}` object, so a strict parse is the whole story; there
/// is deliberately no fence-stripping, prose-skipping or repair here.
fn parse_call(registry: &ToolRegistry, reply: &str) -> Option<ValidatedCall> {
    let value: serde_json::Value = serde_json::from_str(reply.trim()).ok()?;
    registry.validate(&value).ok()
}

/// Enforce [`RESULT_BYTE_CAP`] by cutting the middle out, keeping the head (the
/// command echo, the first error) and the tail (the summary line).
fn cap_result(text: String) -> String {
    if text.len() <= RESULT_BYTE_CAP {
        return text;
    }
    let half = RESULT_BYTE_CAP / 2;
    let head_end = text.floor_char_boundary(half);
    let tail_start = text.ceil_char_boundary(text.len() - half);
    format!(
        "{}\n[... {} bytes cut from the middle: result exceeded the {} byte cap ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        RESULT_BYTE_CAP,
        &text[tail_start..]
    )
}

impl Solver for RawSolver<'_> {
    fn name(&self) -> &str {
        "raw-native"
    }

    fn solve(&self, task: &EvalTask, workspace: &Path) -> Result<()> {
        let registry = raw_registry();
        let constraint = OutputConstraint::Tools(tool_schemas(&registry));
        let counter = TokenCounter::new(self.backend);

        let mut messages = vec![
            Message::system(format!(
                "Task: {}\n\nVerify command: {}\n\n\
                 Use the tools to make the tests pass, then call finish.",
                task.description, task.verify_cmd
            )),
            Message::user(task.description.clone()),
        ];

        let mut metrics = ToolCallMetrics::default();
        let mut steps = 0;
        let mut total_prompt_tokens = 0;
        let mut peak_reply_tokens = 0;
        let mut self_verified = None;

        let stop_reason = loop {
            if steps >= self.cfg.max_steps {
                break "BudgetExhausted";
            }
            steps += 1;

            let mut req = GenerateRequest::new(messages.clone());
            req.max_tokens = self.cfg.response_reserve_tokens;
            req.constraint = Some(constraint.clone());
            total_prompt_tokens += messages
                .iter()
                .map(|m| counter.count(&m.content))
                .sum::<usize>();

            let reply = self.backend.generate(&req)?;
            peak_reply_tokens = peak_reply_tokens.max(counter.count(&reply.content));
            messages.push(Message::assistant(reply.content.clone()));

            let Some(call) = parse_call(&registry, &reply.content) else {
                metrics.record_invalid();
                messages.push(Message::user(NO_CALL_REPLY));
                continue;
            };
            metrics.record_valid();

            let observation = match call.name.as_str() {
                "finish" => break "Finished",
                "run_command" => {
                    let cmd = call.str("command").unwrap_or_default();
                    let r = run_command_in(&Sandbox::Host, workspace, cmd);
                    format!(
                        "run_command {cmd:?} exited {}:\n{}",
                        r.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                        r.output.trim()
                    )
                }
                "run_verification" => {
                    let report = run_verification_in(&Sandbox::Host, workspace, &task.verify_cmd);
                    let green = report.all_green();
                    self_verified = Some(green);
                    if green {
                        break "Verified";
                    }
                    report.observation()
                }
                _ => match execute(&call, workspace) {
                    ToolOutcome::Observation(text) => text,
                    // Only `finish` yields this and it is matched above.
                    ToolOutcome::Finished => break "Finished",
                },
            };
            messages.push(Message::user(cap_result(observation)));
        };

        self.last.set(Some(metrics));
        self.last_run.replace(Some(RunInfo {
            steps,
            stop_reason: stop_reason.to_string(),
            self_verified,
            interventions: 0,
            total_prompt_tokens,
            peak_reply_tokens,
            harness_faults: Vec::new(),
        }));
        Ok(())
    }

    fn last_metrics(&self) -> Option<ToolCallMetrics> {
        self.last.get()
    }

    fn last_run(&self) -> Option<RunInfo> {
        self.last_run.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::{
        CallbackBackend, Capabilities, GenerateResponse, MockBackend, Role, ToolCalling,
    };
    use serde_json::json;

    fn task_in(dir: &Path) -> EvalTask {
        EvalTask {
            id: "raw".into(),
            description: "Fix is_even so even numbers are reported even.".into(),
            fixture: dir.to_path_buf(),
            verify_cmd: "sh test.sh".into(),
            contract_tests: vec!["test.sh".into()],
            solution: None,
            tags: Vec::new(),
            timeout_secs: None,
        }
    }

    /// The red even-parity fixture used across the eval tests.
    fn red_fixture(dir: &Path) {
        std::fs::write(dir.join("impl.sh"), "is_even() { return 1; }\n").unwrap();
        std::fs::write(
            dir.join("test.sh"),
            ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
        )
        .unwrap();
    }

    #[test]
    fn finish_alone_stops_after_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::new([json!({"tool": "finish"}).to_string()]);
        let solver = RawSolver::new(&backend, AgentConfig::default());

        solver.solve(&task_in(dir.path()), dir.path()).unwrap();

        assert_eq!(solver.name(), "raw-native");
        let run = solver.last_run().unwrap();
        assert_eq!(run.steps, 1);
        assert_eq!(run.stop_reason, "Finished");
        assert_eq!(run.self_verified, None, "never ran its own verification");
        assert_eq!(run.interventions, 0);
        assert!(run.harness_faults.is_empty());
        assert!(run.total_prompt_tokens > 0, "the first prompt was counted");
        let m = solver.last_metrics().unwrap();
        assert_eq!((m.valid, m.invalid), (1, 0));
        assert_eq!(backend.remaining(), 0);
    }

    #[test]
    fn write_then_finish_lands_the_file() {
        let dir = tempfile::tempdir().unwrap();
        red_fixture(dir.path());
        let body = "is_even() { [ $(( $1 % 2 )) -eq 0 ]; }\n";
        let backend = MockBackend::new([
            json!({"tool": "write_file", "path": "impl.sh", "content": body}).to_string(),
            json!({"tool": "finish"}).to_string(),
        ]);
        let solver = RawSolver::new(&backend, AgentConfig::default());

        solver.solve(&task_in(dir.path()), dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("impl.sh")).unwrap(),
            body
        );
        let run = solver.last_run().unwrap();
        assert_eq!((run.steps, run.stop_reason.as_str()), (2, "Finished"));
        assert_eq!(solver.last_metrics().unwrap().valid, 2);
    }

    /// A green `run_verification` ends the run: the model checked its own work and
    /// the harness has nothing more to learn from further turns.
    #[test]
    fn green_verification_stops_the_run() {
        let dir = tempfile::tempdir().unwrap();
        red_fixture(dir.path());
        let backend = MockBackend::new([
            json!({
                "tool": "write_file",
                "path": "impl.sh",
                "content": "is_even() { [ $(( $1 % 2 )) -eq 0 ]; }\n"
            })
            .to_string(),
            json!({"tool": "run_verification"}).to_string(),
            // Never reached.
            json!({"tool": "finish"}).to_string(),
        ]);
        let solver = RawSolver::new(&backend, AgentConfig::default());

        solver.solve(&task_in(dir.path()), dir.path()).unwrap();

        let run = solver.last_run().unwrap();
        assert_eq!((run.steps, run.stop_reason.as_str()), (2, "Verified"));
        assert_eq!(run.self_verified, Some(true));
        assert_eq!(backend.remaining(), 1, "stopped before the scripted finish");
    }

    /// A reply with no call is counted invalid, answered with the one fixed line,
    /// and the run carries on -- no repair prompt, no schema re-statement.
    #[test]
    fn a_reply_without_a_call_is_counted_invalid_and_the_run_continues() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::new([
            "I think I should look at the file first.".to_string(),
            json!({"tool": "finish"}).to_string(),
        ]);
        let solver = RawSolver::new(&backend, AgentConfig::default());

        solver.solve(&task_in(dir.path()), dir.path()).unwrap();

        let m = solver.last_metrics().unwrap();
        assert_eq!((m.valid, m.invalid), (1, 1));
        assert_eq!(solver.last_run().unwrap().steps, 2);
    }

    #[test]
    fn the_step_cap_ends_a_run_that_never_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MockBackend::new(std::iter::repeat_n("not a call", 5));
        let cfg = AgentConfig {
            max_steps: 3,
            ..AgentConfig::default()
        };
        let solver = RawSolver::new(&backend, cfg);

        solver.solve(&task_in(dir.path()), dir.path()).unwrap();

        let run = solver.last_run().unwrap();
        assert_eq!(
            (run.steps, run.stop_reason.as_str()),
            (3, "BudgetExhausted")
        );
        assert_eq!(backend.remaining(), 2);
    }

    /// What the model is actually sent: one system message plus the task, the six
    /// tools attached natively, and the configured reply budget. The transcript
    /// then only ever grows.
    #[test]
    fn the_request_is_one_system_message_six_native_tools_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let seen: RefCell<Vec<GenerateRequest>> = RefCell::new(Vec::new());
        let caps = Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::OpenAiStyle,
            on_device: false,
        };
        let backend = CallbackBackend::new("probe", caps, |req: &GenerateRequest| {
            seen.borrow_mut().push(req.clone());
            let n = seen.borrow().len();
            Ok(GenerateResponse::new(if n == 1 {
                json!({"tool": "read_file", "path": "missing.txt"}).to_string()
            } else {
                json!({"tool": "finish"}).to_string()
            }))
        });
        let cfg = AgentConfig {
            response_reserve_tokens: 777,
            ..AgentConfig::default()
        };
        let solver = RawSolver::new(&backend, cfg);
        let task = task_in(dir.path());

        solver.solve(&task, dir.path()).unwrap();

        let seen = seen.borrow();
        assert_eq!(seen.len(), 2);
        let first = &seen[0];
        assert_eq!(first.max_tokens, 777);
        let roles: Vec<Role> = first.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::System, Role::User]);
        assert!(first.messages[0].content.contains("sh test.sh"));
        assert!(first.messages[0].content.contains("then call finish"));
        assert_eq!(first.messages[1].content, task.description);
        let Some(OutputConstraint::Tools(tools)) = &first.constraint else {
            panic!(
                "expected a native Tools constraint, got {:?}",
                first.constraint
            );
        };
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "edit_file",
                "finish",
                "read_file",
                "run_command",
                "run_verification",
                "write_file"
            ]
        );

        // Turn two: the same two messages, then the assistant's call and its
        // result appended -- untouched.
        let second = &seen[1];
        let roles: Vec<Role> = second.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::System, Role::User, Role::Assistant, Role::User]
        );
        assert_eq!(second.messages[0].content, first.messages[0].content);
        assert!(second.messages[3].content.contains("missing.txt"));
    }

    #[test]
    fn a_result_over_the_cap_is_cut_in_the_middle() {
        let text = "a".repeat(10_000) + &"z".repeat(10_000);
        let out = cap_result(text);
        assert!(out.len() < 20_000);
        assert!(out.starts_with(&"a".repeat(8 * 1024)));
        assert!(out.ends_with(&"z".repeat(8 * 1024)));
        assert!(out.contains("bytes cut from the middle"));

        let small = "short".to_string();
        assert_eq!(cap_result(small.clone()), small);
    }

    #[test]
    fn multibyte_text_is_cut_on_char_boundaries() {
        let text = "é".repeat(RESULT_BYTE_CAP);
        let out = cap_result(text);
        assert!(out.contains("bytes cut from the middle"));
        assert!(out.chars().all(|c| c == 'é' || c.is_ascii()));
    }
}
