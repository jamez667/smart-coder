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
    /// The commit the BINARY WAS BUILT FROM ([`current_commit`]) -- `"unknown"`
    /// outside a checkout, with a `-dirty` suffix when it was built from a tree
    /// with uncommitted changes. Two rows with different commits are not the same
    /// experiment.
    ///
    /// Deliberately not the runtime HEAD: HEAD says what the tree was when the
    /// binary RAN, which for a stale binary names code the row does not contain.
    pub commit: String,
    /// The runtime HEAD, when it differed from `commit` -- i.e. the binary was
    /// stale relative to the working tree. `None` when they agreed, which is the
    /// normal case, so a healthy row does not carry a redundant column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_commit: Option<String>,
    /// 1-based round within the run.
    pub repeat: usize,
    /// [`crate::runner::Outcome::symbol`] -- `"PASS"`, `"STILL-RED"`, ...
    pub outcome: String,
    pub steps: usize,
    pub total_prompt_tokens: usize,
    /// Of `total_prompt_tokens`, how many the backend served from its KV cache.
    ///
    /// `total_prompt_tokens` measures what the harness SENT, which the
    /// append-only prompt work cannot move by design -- a byte-stable prefix
    /// sends the same tokens a shifting one does. This is what the server did
    /// with them, and it is where the work becomes visible.
    ///
    /// `0` on an arm whose backend reports no split (raw, pi, and every
    /// non-llama.cpp server), summed off the `ModelTurn` events so an arm that
    /// never reported one stays at zero rather than being credited or blamed.
    pub cached_prompt_tokens: usize,
    /// Of `total_prompt_tokens`, how many the backend actually PREFILLED.
    pub prefilled_prompt_tokens: usize,
    /// `cached / (cached + prefilled)` as a whole percent. `None` when the
    /// backend reported no split at all -- a run against a server that says
    /// nothing must not print `0%`, which is a claim about the cache rather than
    /// about the reporting.
    ///
    /// **This is the number that says whether the append-only prefix works.** A
    /// stable prefix re-prefills only the newly appended tokens each turn, so a
    /// healthy multi-turn run sits high; a run that re-prefills the whole prompt
    /// every turn sits near zero.
    pub cache_hit_percent: Option<u32>,
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
    ///
    /// `tree_commit` is filled in only when the runtime HEAD disagrees with
    /// `commit`, so a stale-binary run is legible in the rows and not only in the
    /// scrollback that carried the warning.
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
            tree_commit: runtime_head().filter(|head| head != commit),
            repeat,
            outcome: result.outcome.symbol().to_string(),
            steps: run.map(|r| r.steps).unwrap_or(0),
            total_prompt_tokens: run.map(|r| r.total_prompt_tokens).unwrap_or(0),
            // From the sink, not the report: the sink sees every `ModelTurn` on
            // every arm, including the ones whose solver builds its own `RunInfo`.
            cached_prompt_tokens: metrics.cached_prompt_tokens,
            prefilled_prompt_tokens: metrics.prefilled_prompt_tokens,
            cache_hit_percent: metrics.cache_hit_percent(),
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

/// The short hash of the commit this binary was **built** from, `-dirty` when it
/// was built from a tree with uncommitted changes, or `"unknown"` when it was
/// built outside a checkout.
///
/// Stamped by `build.rs` at compile time, not read from git at run time. The
/// runtime answer is a different question -- what the tree is NOW -- and using it
/// makes a stale binary claim code it does not contain. Copied into every row: a
/// row without a commit cannot be compared with anything.
pub fn current_commit() -> String {
    option_env!("SC_EVAL_BUILD_COMMIT")
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// `git rev-parse --short HEAD` right now, or `None` outside a checkout.
///
/// Used ONLY to detect that the binary is stale relative to the tree; it never
/// becomes a row's `commit`.
pub fn runtime_head() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The warning to print when a binary is stale relative to the working tree, or
/// `None` when there is nothing to say.
///
/// Pure, so the interesting case is testable without a git checkout in a
/// particular state. Says nothing when the two agree, when there is no runtime
/// HEAD to compare against (not a checkout), or when the build stamp is
/// `"unknown"` -- an unstamped build is already its own kind of loud, and pairing
/// it with a HEAD would suggest a mismatch that was never measured.
///
/// A `-dirty` build stamp never matches a bare HEAD, and that is correct: a binary
/// built from uncommitted work is exactly as unreproducible as a stale one.
pub fn stale_binary_warning(build: &str, head: Option<&str>) -> Option<String> {
    let head = head?;
    if build == "unknown" || build == head {
        return None;
    }
    Some(format!(
        "WARNING: this binary was built from {build} but the working tree is at {head}; \
         rows will be stamped {build}. Rebuild before trusting these numbers as \
         current, or ignore this if the older binary is the point (an A/B against it)."
    ))
}

/// Print [`stale_binary_warning`] to stderr, if there is one. Call once at run
/// start, before anything that produces rows.
pub fn warn_if_stale(build: &str) {
    if let Some(msg) = stale_binary_warning(build, runtime_head().as_deref()) {
        eprintln!("{msg}");
    }
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
    /// Prompt tokens the backend served from its KV cache, summed over the
    /// `ModelTurn` events that reported a split. Stays 0 for an arm whose
    /// backend reports none, which is the honest answer for the raw and pi arms.
    pub cached_prompt_tokens: usize,
    /// Prompt tokens the backend actually prefilled, summed the same way.
    pub prefilled_prompt_tokens: usize,
    /// Whether ANY turn reported a split. Without this, "the server said nothing"
    /// and "nothing was cached" both read as two zeroes.
    pub reported_cache: bool,
}

impl RunMetrics {
    /// The share of the prompt the backend served from cache, as a whole percent.
    /// `None` when no turn reported a split.
    pub fn cache_hit_percent(&self) -> Option<u32> {
        if !self.reported_cache {
            return None;
        }
        let total = self.cached_prompt_tokens + self.prefilled_prompt_tokens;
        (total > 0)
            .then(|| ((self.cached_prompt_tokens as f64 / total as f64) * 100.0).round() as u32)
    }
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
                cached_prompt_tokens,
                prefilled_prompt_tokens,
                ..
            } => {
                st.current_step = *step;
                st.metrics.peak_prompt_tokens = st.metrics.peak_prompt_tokens.max(*prompt_tokens);
                // Summed off the events rather than the report so every arm is
                // measured the same way -- and a turn the backend said nothing
                // about adds nothing, so an arm against a server with no split
                // reporting stays at 0 instead of looking like a total cache miss.
                if cached_prompt_tokens.is_some() || prefilled_prompt_tokens.is_some() {
                    st.metrics.reported_cache = true;
                }
                st.metrics.cached_prompt_tokens += cached_prompt_tokens.unwrap_or(0);
                st.metrics.prefilled_prompt_tokens += prefilled_prompt_tokens.unwrap_or(0);
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
            cached_prompt_tokens: None,
            prefilled_prompt_tokens: None,
            raw: String::new(),
        }
    }

    /// A turn whose backend reported the prefix-cache split.
    fn cached_turn(
        step: usize,
        prompt_tokens: usize,
        cached: usize,
        prefilled: usize,
    ) -> AgentEvent {
        AgentEvent::ModelTurn {
            step,
            prompt_tokens,
            cached_prompt_tokens: Some(cached),
            prefilled_prompt_tokens: Some(prefilled),
            raw: String::new(),
        }
    }

    /// **The sink sums the split, and says nothing when the backend said nothing.**
    ///
    /// The whole point of the column: a run whose prefix held re-prefills only the
    /// appended tokens, so the cached side dominates. An arm against a backend with
    /// no reporting (raw, pi, any non-llama.cpp server) must come out `None`, not
    /// `0%` -- the second is a claim about the cache that nobody measured.
    #[test]
    fn the_sink_sums_the_prefix_cache_split_only_when_reported() {
        let sink = MetricsSink::new();
        sink.record(&cached_turn(1, 200, 0, 200));
        sink.record(&cached_turn(2, 260, 200, 60));
        sink.record(&cached_turn(3, 300, 260, 40));
        let m = sink.snapshot();
        assert_eq!(m.cached_prompt_tokens, 460);
        assert_eq!(m.prefilled_prompt_tokens, 300);
        assert_eq!(m.cache_hit_percent(), Some(61));

        // A backend that reports nothing is unknown, not a miss.
        let quiet = MetricsSink::new();
        quiet.record(&turn(1, 200));
        quiet.record(&turn(2, 400));
        let m = quiet.snapshot();
        assert_eq!(m.cached_prompt_tokens, 0);
        assert_eq!(m.prefilled_prompt_tokens, 0);
        assert_eq!(m.cache_hit_percent(), None);
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

    /// **The stamp is the BUILD commit, not the runtime HEAD.**
    ///
    /// The bug this replaces: a binary built at 08:26 stamped a commit that landed
    /// at 08:56, so rows claimed a fix they did not contain. Testing a build
    /// script's output from inside the crate is necessarily indirect -- the
    /// strongest available assertion is that the function returns exactly what the
    /// build script put in the environment, and that in a checkout it is a real
    /// hash rather than the `"unknown"` fallback.
    #[test]
    fn current_commit_is_the_build_time_stamp() {
        let stamp = current_commit();
        assert!(
            !stamp.is_empty(),
            "a row without a commit compares to nothing"
        );
        assert_eq!(
            stamp,
            env!("SC_EVAL_BUILD_COMMIT"),
            "the stamp must be exactly what build.rs emitted"
        );
        // This crate is built inside a checkout, so the fallback means the build
        // script failed to find git -- which would silently un-pin every row.
        if runtime_head().is_some() {
            assert_ne!(
                stamp, "unknown",
                "built in a checkout: expected a real hash"
            );
            let hash = stamp.strip_suffix("-dirty").unwrap_or(&stamp);
            assert!(
                hash.len() >= 7 && hash.chars().all(|c| c.is_ascii_hexdigit()),
                "expected a short hash, optionally `-dirty`: {stamp:?}"
            );
        }
    }

    /// The dirty marker: a build from an uncommitted tree is as unreproducible as
    /// a stale binary, so it must not look like a clean build of that commit.
    /// Which case this run is depends on the tree, so assert the property that
    /// holds either way, and the marker's consequence where it applies.
    #[test]
    fn a_dirty_build_is_marked_and_never_matches_a_bare_head() {
        let stamp = current_commit();
        if let Some(dirty_of) = stamp.strip_suffix("-dirty") {
            assert!(!dirty_of.is_empty(), "`-dirty` must qualify a hash");
            // The point of the marker: it can never be mistaken for the commit.
            assert!(
                stale_binary_warning(&stamp, Some(dirty_of)).is_some(),
                "a dirty build against its own HEAD must still warn"
            );
        }
        // Whatever the tree, the marker is the only suffix the stamp may carry.
        let bare = stamp.strip_suffix("-dirty").unwrap_or(&stamp);
        assert!(!bare.contains('-'), "unexpected suffix on {stamp:?}");
    }

    /// **The mismatch is loud.** Pure comparison, so the stale case is testable
    /// without contriving a git checkout.
    #[test]
    fn a_stale_binary_warns_and_a_current_one_does_not() {
        // Built from one commit, run against a tree at another: the whole bug.
        let msg = stale_binary_warning("aaa1111", Some("c421932"))
            .expect("a mismatch must produce a warning");
        assert!(msg.starts_with("WARNING:"), "must be unmissable: {msg:?}");
        assert!(msg.contains("aaa1111") && msg.contains("c421932"));
        assert!(
            msg.contains("stamped aaa1111"),
            "must say which commit the rows get: {msg:?}"
        );

        // Agreement is the normal case and says nothing.
        assert_eq!(stale_binary_warning("aaa1111", Some("aaa1111")), None);
        // Not a checkout: nothing to compare against, so no claim either way.
        assert_eq!(stale_binary_warning("aaa1111", None), None);
        // An unstamped build is its own problem; pairing it with a HEAD would
        // report a mismatch nobody measured.
        assert_eq!(stale_binary_warning("unknown", Some("c421932")), None);
        // A dirty build is never "current", even at the same commit.
        assert!(stale_binary_warning("aaa1111-dirty", Some("aaa1111")).is_some());
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
                total_cached_prompt_tokens: 10_000,
                total_prefilled_prompt_tokens: 2_000,
                peak_reply_tokens: 900,
                harness_faults: vec![(sc_core::FaultKind::ReplyTruncated, 2)],
            }),
        };
        let metrics = RunMetrics {
            turns_to_first_edit: Some(3),
            re_reads: 1,
            wasted_turns: 0,
            peak_prompt_tokens: 4_000,
            cached_prompt_tokens: 10_000,
            prefilled_prompt_tokens: 2_000,
            reported_cache: true,
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
        // The cache split comes off the sink and the percentage is derived from it.
        assert_eq!(row.cached_prompt_tokens, 10_000);
        assert_eq!(row.prefilled_prompt_tokens, 2_000);
        assert_eq!(row.cache_hit_percent, Some(83));
        assert_eq!(row.faults, vec![("reply truncated".to_string(), 2)]);
        assert_eq!(row.turns_to_first_edit, Some(3));

        let json = serde_json::to_string(&row).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["task"], "t");
        assert_eq!(v["repeat"], 2);
        assert_eq!(v["wall_ms"], 1500);
        assert_eq!(v["outcome"], "PASS");
    }

    /// **A row records the BUILD stamp**, and carries the runtime HEAD beside it
    /// only when they disagree -- so a stale-binary run stays legible in the data
    /// after the scrollback carrying the warning is gone.
    #[test]
    fn a_row_records_the_build_stamp_and_flags_a_stale_tree() {
        let t = task(&[]);
        let result = TaskResult {
            id: "t".into(),
            solver: "raw".into(),
            outcome: Outcome::StillRed,
            metrics: None,
            run: None,
        };
        let stamp = current_commit();
        let row = ResultRow::new(
            &t,
            "raw",
            "m",
            &stamp,
            1,
            &result,
            &RunMetrics::default(),
            0,
        );
        assert_eq!(
            row.commit, stamp,
            "the row carries the build stamp verbatim"
        );
        // Whether THIS binary is stale depends on the tree it was built in, so the
        // invariant to assert is the equivalence, not either branch: the column is
        // present exactly when the stamp and the tree disagree.
        assert_eq!(
            row.tree_commit.is_some(),
            runtime_head().is_some_and(|head| head != stamp),
            "tree_commit appears exactly when the binary is stale (or built dirty)"
        );

        // A row whose stamp agrees with the tree omits the column rather than
        // writing a null -- the healthy case stays as narrow as it was before.
        let agreeing = runtime_head().unwrap_or_else(|| stamp.clone());
        let clean = ResultRow::new(
            &t,
            "raw",
            "m",
            &agreeing,
            1,
            &result,
            &RunMetrics::default(),
            0,
        );
        assert_eq!(clean.tree_commit, None);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&clean).unwrap()).unwrap();
        assert_eq!(v["commit"], agreeing.as_str());
        assert!(v.get("tree_commit").is_none());

        // A row stamped with some OTHER build carries the tree it actually ran on.
        let stale = ResultRow::new(
            &t,
            "raw",
            "m",
            "0000000",
            1,
            &result,
            &RunMetrics::default(),
            0,
        );
        assert_eq!(
            stale.commit, "0000000",
            "the stamp wins; HEAD never overwrites it"
        );
        assert_eq!(
            stale.tree_commit,
            runtime_head(),
            "a disagreement is recorded, not silently dropped"
        );
        if let Some(head) = runtime_head() {
            let v: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&stale).unwrap()).unwrap();
            assert_eq!(v["commit"], "0000000");
            assert_eq!(
                v["tree_commit"], head,
                "the stale run stays legible in the data, not only in the scrollback"
            );
        }
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
