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
use dc_tools::{execute, Journal, PermissionPolicy, ToolOutcome, ToolRegistry};

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
    /// The permission gate consulted before every mutating/destructive call
    /// (spec 04). Defaults conservatively: edits auto, shell denied, frozen tests
    /// untouchable.
    pub permission: PermissionPolicy,
    /// The project's test command. When set, the loop runs verify-red-first and
    /// gates `finish` on a green whole suite (spec 11). `run_verification` uses it.
    pub verify_command: Option<String>,
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
            permission: PermissionPolicy::default(),
            verify_command: None,
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
    /// Whether the configured verification command was green at `finish` (spec 11
    /// — the whole-suite gate). `None` if no `verify_command` was configured.
    pub verified: Option<bool>,
    /// A compact summary of files changed over the run (spec 04/06 — the journal's
    /// diff overview).
    pub change_summary: String,
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
    // Records every mutation for the diff overview + rollback (spec 04).
    let mut journal = Journal::new();

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
                // Snapshot before a mutating, path-bearing call so the journal can
                // record the change (and roll back) after it runs (spec 04).
                let pre = mutating_path(&call, registry)
                    .map(|p| (p.clone(), Journal::snapshot(workspace, &p)));
                let outcome = dispatch(
                    &call,
                    registry,
                    &cfg.permission,
                    &cfg.verify_command,
                    workspace,
                );
                if let Some((path, before)) = pre {
                    journal.record(workspace, &path, before);
                }
                match outcome {
                    ToolOutcome::Finished => {
                        // Whole-suite gate (spec 11): if a verification command is
                        // configured, `finish` is only honored when it's green.
                        match gate_finish(&cfg.verify_command, workspace) {
                            FinishGate::Allow(verified) => {
                                return Ok(AgentReport {
                                    finished: true,
                                    steps: step + 1,
                                    metrics,
                                    peak_prompt_tokens,
                                    prompt_budget: budget,
                                    verified,
                                    change_summary: journal.change_summary(),
                                })
                            }
                            // Tests still red — refuse finish, feed failures back.
                            FinishGate::Refuse(obs) => {
                                history.push(TurnRecord::new("finish (refused)", "", true));
                                push_recent(
                                    &mut recent,
                                    &resp.content,
                                    &truncate_observation(&obs, cfg.observation_line_cap, true),
                                    cfg.keep_recent_turns,
                                );
                            }
                        }
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
        verified: cfg
            .verify_command
            .as_ref()
            .map(|c| dc_verify::run_verification(workspace, c).all_green()),
        change_summary: journal.change_summary(),
    })
}

/// Outcome of the whole-suite gate at `finish`.
enum FinishGate {
    /// Finish is honored; the bool is the verified state (None → no verify cmd).
    Allow(Option<bool>),
    /// Finish is refused with an observation the model must react to.
    Refuse(String),
}

/// Run the configured verification before honoring `finish` (spec 11). With no
/// command configured, finish is always allowed (verified = None).
fn gate_finish(verify_command: &Option<String>, workspace: &Path) -> FinishGate {
    match verify_command {
        None => FinishGate::Allow(None),
        Some(cmd) => {
            let report = dc_verify::run_verification(workspace, cmd);
            if report.all_green() {
                FinishGate::Allow(Some(true))
            } else {
                FinishGate::Refuse(format!(
                    "cannot finish yet — the suite is not green:\n{}",
                    report.observation()
                ))
            }
        }
    }
}

/// Execute a validated call: enforce the permission gate (spec 04), then route
/// to the right executor. `find_symbol` goes to the retrieval index and
/// `run_command`/`run_verification` to dc-verify (neither belongs in the pure-fs
/// tool registry); everything else is the registry's `execute`.
fn dispatch(
    call: &dc_tools::ValidatedCall,
    registry: &ToolRegistry,
    policy: &PermissionPolicy,
    verify_command: &Option<String>,
    workspace: &Path,
) -> ToolOutcome {
    // Permission gate — the harness decides, outside the model's control (spec 04).
    if let Some(spec) = registry.get(&call.name) {
        if let dc_tools::Decision::Deny(reason) = policy.check(call, spec.side_effect) {
            return ToolOutcome::Observation(format!("{} denied: {reason}", call.name));
        }
    }

    match call.name.as_str() {
        "find_symbol" => {
            let name = call.str("name").unwrap_or_default();
            ToolOutcome::Observation(dc_index::find_symbol(workspace, name))
        }
        "run_command" => {
            let cmd = call.str("command").unwrap_or_default();
            let r = dc_verify::run_command(workspace, cmd);
            ToolOutcome::Observation(format!(
                "run_command {cmd:?} exited {}:\n{}",
                r.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                r.output.trim()
            ))
        }
        "run_verification" => match verify_command {
            Some(cmd) => {
                ToolOutcome::Observation(dc_verify::run_verification(workspace, cmd).observation())
            }
            None => ToolOutcome::Observation(
                "run_verification: no verification command is configured for this project".into(),
            ),
        },
        _ => execute(call, workspace),
    }
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

/// If `call` is a mutating, path-bearing tool, return its workspace-relative
/// path (so the journal can snapshot it). `run_verification`/`run_command` are
/// mutating-ish but have no single file to record.
fn mutating_path(call: &dc_tools::ValidatedCall, registry: &ToolRegistry) -> Option<String> {
    let spec = registry.get(&call.name)?;
    if spec.side_effect != dc_tools::SideEffect::Mutating {
        return None;
    }
    call.str("path").map(|s| s.to_string())
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
