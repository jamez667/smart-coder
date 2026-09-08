//! One row per (task, arm, repeat), written as JSON lines so a run's numbers
//! survive the terminal.
//!
//! The A/B used to print a scorecard and exit; the only record of a run was
//! scrollback. A row is the same numbers made durable and comparable across
//! runs: the commit and model pin what was measured, and the process metrics
//! ([`MetricsSink`]) say HOW a task was solved, not only whether.
//!
//! # What the process metrics mean
//!
//! * `turns_to_first_edit` -- the step of the first mutating tool call. A model
//!   that reads for twenty turns before touching a file is deliberating, and a
//!   solve rate cannot see that.
//! * `re_reads` -- a read-only call the model had already made, verbatim. The
//!   symptom of harness amnesia (the prompt shrinking between turns) as much as
//!   of model thrash, and the two are told apart by looking at the prompt size
//!   next to it.
//! * `wasted_turns` -- stalls and repairs: turns that produced no action.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sc_core::{AgentEvent, EventSink};
use serde::Serialize;

use crate::runner::TaskResult;
use crate::task::EvalTask;

/// One (task, arm, repeat) measurement.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResultRow {
    pub task: String,
    pub arm: String,
    pub model: String,
    /// `git rev-parse --short HEAD` at the time of the run, `"unknown"` outside a
    /// checkout. Two rows with different commits are not the same experiment.
    pub commit: String,
    /// 1-based round within the run.
    pub repeat: usize,
    /// [`crate::runner::Outcome::symbol`] -- `"PASS"`, `"STILL-RED"`, ...
    pub outcome: String,
    pub steps: usize,
    pub total_prompt_tokens: usize,
    pub peak_prompt_tokens: usize,
    pub peak_reply_tokens: usize,
    pub wall_ms: u128,
    /// Harness faults by label, most frequent first.
    pub faults: Vec<(String, usize)>,
    pub interventions: usize,
    pub turns_to_first_edit: Option<usize>,
    pub re_reads: usize,
    pub wasted_turns: usize,
    /// The `rung:` tag of the task, when it has one.
    pub rung: Option<String>,
}

impl ResultRow {
    /// Assemble a row from a scored task and the metrics observed while solving it.
    ///
    /// `arm` is the arm's label, `commit` comes from [`current_commit`] once per
    /// run, and `metrics` is the [`MetricsSink`] snapshot for this one solve.
    #[allow(clippy::too_many_arguments)] // a record constructor; every input is a column
    pub fn new(
        task: &EvalTask,
        arm: &str,
        model: &str,
        commit: &str,
        repeat: usize,
        result: &TaskResult,
        metrics: &RunMetrics,
        wall_ms: u128,
    ) -> Self {
        let run = result.run.as_ref();
        let mut faults: Vec<(String, usize)> = run
            .map(|r| {
                r.harness_faults
                    .iter()
                    .map(|(k, n)| (k.label().to_string(), *n))
                    .collect()
            })
            .unwrap_or_default();
        faults.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Self {
            task: task.id.clone(),
            arm: arm.to_string(),
            model: model.to_string(),
            commit: commit.to_string(),
            repeat,
            outcome: result.outcome.symbol().to_string(),
            steps: run.map(|r| r.steps).unwrap_or(0),
            total_prompt_tokens: run.map(|r| r.total_prompt_tokens).unwrap_or(0),
            // The agent report carries a peak too, but `RunInfo` does not, and the
            // sink sees every `ModelTurn`, so the two agree by construction.
            peak_prompt_tokens: metrics.peak_prompt_tokens,
            peak_reply_tokens: run.map(|r| r.peak_reply_tokens).unwrap_or(0),
            wall_ms,
            faults,
            interventions: run.map(|r| r.interventions).unwrap_or(0),
            turns_to_first_edit: metrics.turns_to_first_edit,
            re_reads: metrics.re_reads,
            wasted_turns: metrics.wasted_turns,
            rung: rung_of(task),
        }
    }

    pub fn is_pass(&self) -> bool {
        self.outcome == "PASS"
    }
}

/// The `rung:` tag of a task, without the prefix.
pub fn rung_of(task: &EvalTask) -> Option<String> {
    task.tags
        .iter()
        .find_map(|t| t.strip_prefix("rung:"))
        .map(str::to_string)
}

/// The short hash of the checked-out commit, or `"unknown"`.
///
/// Called once per run and copied into every row: a row without a commit cannot
/// be compared with anything.
pub fn current_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Append `rows` to `<dir>/rows.jsonl`, one JSON object per line, creating the
/// directory if needed. Returns the file written.
///
/// Appends rather than overwrites so repeated runs accumulate into one file that
/// can be grouped by commit and model afterwards.
pub fn write_rows(dir: &Path, rows: &[ResultRow]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("rows.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    for row in rows {
        let line = serde_json::to_string(row).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    file.flush()?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// The metrics sink.
// ---------------------------------------------------------------------------

/// Tools whose call is an edit: the first of these marks `turns_to_first_edit`.
const EDIT_TOOLS: [&str; 6] = [
    "write_file",
    "create_file",
    "edit_file",
    "append_file",
    "edit_lines",
    "edit_function",
];

/// Tools whose repeated (tool, arg) call is a re-read.
const READ_TOOLS: [&str; 4] = ["read_file", "read_function", "search_code", "list_dir"];

/// What the sink observed over one solve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMetrics {
    /// Step (1-based) of the first mutating tool call; `None` if it never edited.
    pub turns_to_first_edit: Option<usize>,
    /// Read-only calls repeated verbatim.
    pub re_reads: usize,
    /// `Stalled` plus `RepairTriggered` events.
    pub wasted_turns: usize,
    /// The largest assembled prompt, from the `ModelTurn` events.
    pub peak_prompt_tokens: usize,
}

#[derive(Debug, Default)]
struct State {
    metrics: RunMetrics,
    /// The step of the most recent `ModelTurn`, which precedes that turn's
    /// `ToolCall` -- the loop emits the reply before dispatching it.
    current_step: usize,
    seen_reads: Vec<(String, String)>,
}

/// An [`EventSink`] that counts how a task was solved.
///
/// Interior mutability because the loop takes `&dyn EventSink`. Reset it between
/// tasks with [`MetricsSink::reset`] and read the counts with
/// [`MetricsSink::snapshot`]; it is one sink per arm, reused across tasks.
#[derive(Debug, Default)]
pub struct MetricsSink {
    state: Mutex<State>,
}

impl MetricsSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything: call before each solve.
    pub fn reset(&self) {
        *self.state.lock().expect("metrics lock") = State::default();
    }

    /// The counts since the last [`MetricsSink::reset`].
    pub fn snapshot(&self) -> RunMetrics {
        self.state.lock().expect("metrics lock").metrics.clone()
    }

    /// Reset and return what was observed, in one step.
    pub fn take(&self) -> RunMetrics {
        std::mem::take(&mut *self.state.lock().expect("metrics lock")).metrics
    }
}

impl EventSink for MetricsSink {
    fn record(&self, event: &AgentEvent) {
        let mut st = self.state.lock().expect("metrics lock");
        match event {
            AgentEvent::ModelTurn {
                step,
                prompt_tokens,
                ..
            } => {
                st.current_step = *step;
                st.metrics.peak_prompt_tokens = st.metrics.peak_prompt_tokens.max(*prompt_tokens);
            }
            AgentEvent::ToolCall { tool, arg } => {
                if EDIT_TOOLS.contains(&tool.as_str()) {
                    if st.metrics.turns_to_first_edit.is_none() {
                        // A `ToolCall` with no preceding `ModelTurn` (a scripted
                        // backend, say) still counts as the first turn.
                        st.metrics.turns_to_first_edit = Some(st.current_step.max(1));
                    }
                } else if READ_TOOLS.contains(&tool.as_str()) {
                    let key = (tool.clone(), arg.clone());
                    if st.seen_reads.contains(&key) {
                        st.metrics.re_reads += 1;
                    } else {
                        st.seen_reads.push(key);
                    }
                }
            }
            AgentEvent::Stalled { .. } | AgentEvent::RepairTriggered { .. } => {
                st.metrics.wasted_turns += 1;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Outcome;
    use crate::solver::RunInfo;

    fn turn(step: usize, prompt_tokens: usize) -> AgentEvent {
        AgentEvent::ModelTurn {
            step,
            prompt_tokens,
            raw: String::new(),
        }
    }

    fn call(tool: &str, arg: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            tool: tool.into(),
            arg: arg.into(),
        }
    }

    #[test]
    fn first_edit_is_attributed_to_the_turn_that_made_it() {
        let sink = MetricsSink::new();
        sink.record(&turn(1, 100));
        sink.record(&call("read_file", "a.rs"));
        sink.record(&turn(2, 300));
        sink.record(&call("run_command", "cargo test"));
        sink.record(&turn(3, 250));
        sink.record(&call("edit_file", "a.rs"));
        sink.record(&turn(4, 400));
        sink.record(&call("write_file", "b.rs"));
        let m = sink.snapshot();
        assert_eq!(m.turns_to_first_edit, Some(3));
        assert_eq!(m.peak_prompt_tokens, 400);
        assert_eq!(m.re_reads, 0);
    }

    #[test]
    fn a_verbatim_repeat_of_a_read_is_a_re_read() {
        let sink = MetricsSink::new();
        sink.record(&call("read_file", "a.rs"));
        sink.record(&call("read_file", "b.rs"));
        sink.record(&call("read_file", "a.rs")); // re-read
        sink.record(&call("search_code", "fn max"));
        sink.record(&call("search_code", "fn max")); // re-read
        sink.record(&call("search_code", "fn min")); // different arg: not
                                                     // Edits are never re-reads, however often they repeat.
        sink.record(&call("edit_file", "a.rs"));
        sink.record(&call("edit_file", "a.rs"));
        let m = sink.snapshot();
        assert_eq!(m.re_reads, 2);
        assert_eq!(
            m.turns_to_first_edit,
            Some(1),
            "no ModelTurn seen: first turn"
        );
    }

    #[test]
    fn stalls_and_repairs_are_wasted_turns_and_reset_clears_them() {
        let sink = MetricsSink::new();
        sink.record(&AgentEvent::Stalled {
            trigger: "loop".into(),
        });
        sink.record(&AgentEvent::RepairTriggered {
            detail: "no json".into(),
        });
        sink.record(&AgentEvent::Stalled {
            trigger: "loop".into(),
        });
        assert_eq!(sink.snapshot().wasted_turns, 3);
        assert_eq!(sink.take().wasted_turns, 3);
        assert_eq!(sink.snapshot(), RunMetrics::default());
    }

    #[test]
    fn a_run_that_never_edits_has_no_first_edit() {
        let sink = MetricsSink::new();
        sink.record(&turn(1, 10));
        sink.record(&call("read_file", "a.rs"));
        sink.record(&call("finish", ""));
        assert_eq!(sink.snapshot().turns_to_first_edit, None);
    }

    fn task(tags: &[&str]) -> EvalTask {
        EvalTask {
            id: "t".into(),
            description: String::new(),
            fixture: PathBuf::new(),
            verify_cmd: String::new(),
            contract_tests: Vec::new(),
            solution: None,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            timeout_secs: None,
        }
    }

    #[test]
    fn the_rung_comes_from_the_tag() {
        assert_eq!(
            rung_of(&task(&["lang:rust", "rung:located"])).as_deref(),
            Some("located")
        );
        assert_eq!(rung_of(&task(&["lang:rust"])), None);
    }

    #[test]
    fn a_row_carries_the_run_and_the_metrics_and_round_trips_as_json() {
        let t = task(&["rung:stated"]);
        let result = TaskResult {
            id: "t".into(),
            solver: "control(6)".into(),
            outcome: Outcome::Pass,
            metrics: None,
            run: Some(RunInfo {
                steps: 7,
                stop_reason: "Finished".into(),
                self_verified: Some(true),
                interventions: 1,
                total_prompt_tokens: 12_000,
                peak_reply_tokens: 900,
                harness_faults: vec![(sc_core::FaultKind::ReplyTruncated, 2)],
            }),
        };
        let metrics = RunMetrics {
            turns_to_first_edit: Some(3),
            re_reads: 1,
            wasted_turns: 0,
            peak_prompt_tokens: 4_000,
        };
        let row = ResultRow::new(
            &t,
            "control(6)",
            "tiel",
            "abc1234",
            2,
            &result,
            &metrics,
            1500,
        );
        assert!(row.is_pass());
        assert_eq!(row.rung.as_deref(), Some("stated"));
        assert_eq!(row.steps, 7);
        assert_eq!(row.peak_prompt_tokens, 4_000);
        assert_eq!(row.faults, vec![("reply truncated".to_string(), 2)]);
        assert_eq!(row.turns_to_first_edit, Some(3));

        let json = serde_json::to_string(&row).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["task"], "t");
        assert_eq!(v["repeat"], 2);
        assert_eq!(v["wall_ms"], 1500);
        assert_eq!(v["outcome"], "PASS");
    }

    #[test]
    fn a_row_for_a_run_that_never_started_is_all_zeroes() {
        let t = task(&[]);
        let result = TaskResult {
            id: "t".into(),
            solver: "raw".into(),
            outcome: Outcome::HarnessError("boom".into()),
            metrics: None,
            run: None,
        };
        let row = ResultRow::new(&t, "raw", "m", "c", 1, &result, &RunMetrics::default(), 0);
        assert_eq!(row.outcome, "HARNESS-ERR");
        assert_eq!(row.steps, 0);
        assert!(row.faults.is_empty());
        assert_eq!(row.rung, None);
    }

    #[test]
    fn write_rows_appends_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let t = task(&[]);
        let result = TaskResult {
            id: "t".into(),
            solver: "raw".into(),
            outcome: Outcome::StillRed,
            metrics: None,
            run: None,
        };
        let row = ResultRow::new(&t, "raw", "m", "c", 1, &result, &RunMetrics::default(), 3);
        let path = write_rows(dir.path(), std::slice::from_ref(&row)).unwrap();
        write_rows(dir.path(), &[row]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "two writes, two lines: {text:?}");
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["arm"], "raw");
        }
    }
}
