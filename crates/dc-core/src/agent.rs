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

use dc_context::{
    prompt_budget, summarize_history, truncate_observation, ContextBuilder, Segment, TokenCounter,
    TurnRecord, Zone,
};
use dc_index::Boosts;
use dc_model::{GenerateRequest, Message, ModelBackend};
use dc_proto::Result;
use dc_tools::{execute, ToolOutcome, ToolRegistry};

use crate::metrics::ToolCallMetrics;
use crate::strategy::ToolCallStrategy;

/// Loop configuration, including the Context Manager's budget knobs (spec 05).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on model turns (spec 03 — budgets are first-class).
    pub max_steps: usize,
    /// Fraction of the backend's advertised window we actually budget against —
    /// small models degrade before the nominal max (spec 05).
    pub effective_context_fraction: f64,
    /// Tokens reserved for the model's reply (subtracted from the budget).
    pub response_reserve_tokens: usize,
    /// Max lines kept from any single tool observation before truncation (spec 05).
    pub observation_line_cap: usize,
    /// How many most-recent turns stay verbatim before older ones are compacted
    /// into a rolling summary (spec 05).
    pub keep_recent_turns: usize,
    /// How many top-ranked symbols the repo map injects into the retrieved zone.
    pub repo_map_top_k: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 12,
            effective_context_fraction: 0.75,
            response_reserve_tokens: 1024,
            observation_line_cap: 40,
            keep_recent_turns: 3,
            repo_map_top_k: 30,
        }
    }
}

/// What happened over a run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReport {
    /// Whether the model called `finish` within budget.
    pub finished: bool,
    /// Model turns taken.
    pub steps: usize,
    /// Tool-call validity metrics over the run (spec 07 — the M1 ≥95% target).
    pub metrics: ToolCallMetrics,
    /// The largest assembled-prompt token count over the run, and the hard budget
    /// it was kept under (spec 05 — the window is a hard-budgeted resource).
    pub peak_prompt_tokens: usize,
    pub prompt_budget: usize,
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

    // Token accounting + hard budget (spec 05). Budget against an effective
    // fraction of the advertised window, minus the response reserve.
    let counter = TokenCounter::new(backend);
    let caps = backend.capabilities();
    let budget = prompt_budget(
        caps.max_context_tokens,
        cfg.effective_context_fraction,
        cfg.response_reserve_tokens,
    );
    let builder = ContextBuilder::new(&counter, budget);

    // The repo map is retrieval that's stable across the run; boost symbols the
    // task names so the map's top is relevant to *this* task (spec 05, aider).
    let repo_map = dc_index::repo_map(
        workspace,
        &Boosts {
            mentioned_symbols: mentioned_identifiers(instruction),
            in_play_files: Vec::new(),
        },
        cfg.repo_map_top_k,
    );

    let mut metrics = ToolCallMetrics::default();
    let mut history: Vec<TurnRecord> = Vec::new();
    // Recent verbatim turns: the assistant's last action and the observation it
    // produced, kept as raw messages for the prompt's recent zone.
    let mut recent: Vec<Message> = Vec::new();
    let mut peak_prompt_tokens = 0usize;

    for step in 0..cfg.max_steps {
        // Compact older turns into a rolling summary; keep the recent ones.
        let (older, _recent_records) =
            dc_context::split_for_compaction(&history, cfg.keep_recent_turns);
        let summary = summarize_history(older);

        // Assemble the budgeted, zoned prompt (spec 05).
        let mut segments = vec![
            Segment::system(Zone::System, system.clone()),
            Segment::user(Zone::TaskAnchor, instruction.to_string()),
        ];
        if !repo_map.is_empty() {
            segments.push(Segment::user(Zone::Retrieved, repo_map.clone()));
        }
        if !summary.is_empty() {
            segments.push(Segment::user(Zone::HistorySummary, summary));
        }
        // The most-recent turns are the sacred RecentObservation zone.
        for (i, m) in recent.iter().enumerate() {
            let zone = if i + 1 == recent.len() {
                Zone::RecentObservation
            } else {
                Zone::HistorySummary // older-but-still-verbatim degrades first
            };
            segments.push(seg_from_message(zone, m));
        }

        let built = builder.build(segments);
        peak_prompt_tokens = peak_prompt_tokens.max(built.tokens_used);

        let mut req = GenerateRequest::new(built.messages);
        strategy.prepare_request(&mut req, registry);
        let resp = backend.generate(&req)?;

        // Decode the tool call.
        match strategy.extract(&resp.content, registry) {
            Ok(call) => {
                metrics.record_valid();
                let arg = key_arg(&call);
                match dispatch(&call, workspace) {
                    ToolOutcome::Finished => {
                        return Ok(AgentReport {
                            finished: true,
                            steps: step + 1,
                            metrics,
                            peak_prompt_tokens,
                            prompt_budget: budget,
                        })
                    }
                    ToolOutcome::Observation(o) => {
                        let was_error = looks_like_failure(&o);
                        let obs = truncate_observation(&o, cfg.observation_line_cap, true);
                        history.push(TurnRecord::new(call.name.clone(), arg, was_error));
                        push_recent(&mut recent, &resp.content, &obs, cfg.keep_recent_turns);
                    }
                }
            }
            // Repair loop: feed back the exact error; never execute a bad call.
            Err(e) => {
                metrics.record_invalid();
                let obs = e.repair_prompt();
                history.push(TurnRecord::new("(malformed)", "", true));
                push_recent(&mut recent, &resp.content, &obs, cfg.keep_recent_turns);
            }
        }
    }

    Ok(AgentReport {
        finished: false,
        steps: cfg.max_steps,
        metrics,
        peak_prompt_tokens,
        prompt_budget: budget,
    })
}

/// Execute a validated call, routing `find_symbol` to the retrieval index (which
/// the tool registry can't execute without depending on tree-sitter).
fn dispatch(call: &dc_tools::ValidatedCall, workspace: &Path) -> ToolOutcome {
    if call.name == "find_symbol" {
        let name = call.str("name").unwrap_or_default();
        return ToolOutcome::Observation(dc_index::find_symbol(workspace, name));
    }
    execute(call, workspace)
}

/// Append the assistant action + its observation, capping the verbatim window to
/// roughly `keep_recent` turns (each turn is one assistant + one user message).
fn push_recent(recent: &mut Vec<Message>, action: &str, observation: &str, keep_recent: usize) {
    recent.push(Message::assistant(action.to_string()));
    recent.push(Message::user(observation.to_string()));
    let max_msgs = keep_recent.saturating_mul(2).max(2);
    while recent.len() > max_msgs {
        recent.remove(0);
    }
}

fn seg_from_message(zone: Zone, m: &Message) -> Segment {
    match m.role {
        dc_model::Role::System => Segment::system(zone, m.content.clone()),
        dc_model::Role::User => Segment::user(zone, m.content.clone()),
        dc_model::Role::Assistant => Segment::assistant(zone, m.content.clone()),
    }
}

/// The key argument of a call, for the history record (path or query/name).
fn key_arg(call: &dc_tools::ValidatedCall) -> String {
    for k in ["path", "query", "name"] {
        if let Some(v) = call.str(k) {
            return v.to_string();
        }
    }
    String::new()
}

/// Does an observation read like a failure the model must react to?
fn looks_like_failure(obs: &str) -> bool {
    let l = obs.to_ascii_lowercase();
    l.contains("error")
        || l.contains("rejected")
        || l.contains("not found")
        || l.contains("no match")
}

/// Crude identifier extraction from the task text, to boost the repo map toward
/// symbols the user actually named (spec 05). Splits on non-identifier chars and
/// keeps word-ish tokens of length ≥ 3.
fn mentioned_identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush_ident(&mut cur, &mut out);
        }
    }
    flush_ident(&mut cur, &mut out);
    out
}

fn flush_ident(cur: &mut String, out: &mut Vec<String>) {
    if cur.len() >= 3 && !out.contains(cur) {
        out.push(cur.clone());
    }
    cur.clear();
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

        let cfg = AgentConfig {
            max_steps: 3,
            ..Default::default()
        };
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
