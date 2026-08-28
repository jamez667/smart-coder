//! What the loop is configured with ([`AgentConfig`]) and what it reports
//! ([`AgentReport`]), plus the two task-prefix system prompts.

use std::sync::Arc;

use sc_tools::PermissionPolicy;

use crate::confirm::Confirmer;
use crate::metrics::ToolCallMetrics;
use crate::recovery::StopReason;

/// Loop configuration, including the Context Manager's budget knobs (spec 05).
///
/// `Debug` is hand-written (below) rather than derived because [`confirmer`] is a
/// trait object, which is not `Debug`.
///
/// [`confirmer`]: AgentConfig::confirmer
#[derive(Clone)]
pub struct AgentConfig {
    /// Hard cap on model turns (spec 03 — budgets are first-class).
    pub max_steps: usize,
    /// Fraction of the backend's advertised window we actually budget against —
    /// small models degrade before the nominal max (spec 05).
    pub effective_context_fraction: f64,
    /// Tokens reserved for the model's reply (subtracted from the budget).
    pub response_reserve_tokens: usize,
    /// Max lines kept from any single tool observation before truncation (spec 05). This
    /// is the cap for runaway command/test logs (a 5k-line pytest dump), where error-first
    /// truncation keeps the signal. File reads use the more generous `read_file_line_cap`.
    pub observation_line_cap: usize,
    /// Max lines kept from a `read_file` observation. A source file is not a runaway log —
    /// clipping it to `observation_line_cap` (40) amputates the very code the model must
    /// edit, so it re-reads or guesses. Give file reads real room to hold whole small/medium
    /// files; the general `observation_line_cap` still tames noisy command output.
    pub read_file_line_cap: usize,
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
    /// Ask the planner for a step plan before the loop (spec 03 — PLAN). When
    /// false, the agent runs plan-free (M0–M3 behavior).
    pub plan_first: bool,
    /// Consecutive identical actions before the harness intervenes (spec 03 — loop
    /// detection).
    pub repeat_limit: usize,
    /// Consecutive turns with no workspace change before intervening (stall).
    pub no_progress_limit: usize,
    /// Per-step retry budget: failed attempts on the active step before the
    /// harness gives up on it and moves on (spec 03).
    pub step_retry_budget: usize,
    /// An optional string appended to the system prompt — a model-quirk hook. Some
    /// small models need a directive to behave (e.g. Qwen3 needs `/no_think` or it
    /// burns its budget in a reasoning block and returns empty). Kept generic so
    /// the harness stays model-agnostic; the CLI sets it per model.
    pub system_suffix: Option<String>,
    /// Files the agent is scoped to edit. When set, the loop pins their *current*
    /// contents (re-read fresh every turn) into the retrieved zone, so a small model
    /// always has a correct, up-to-date view to anchor `edit_file` on without having
    /// to re-read — and, crucially, without the view ever going stale after an edit.
    /// Empty = no focus (the model navigates with read_file as usual). Set by the
    /// swarm, which scopes each worker to a disjoint set of files.
    pub focus_files: Vec<String>,
    /// Plan/preview only: when set, the loop runs read-only tools for real (so the
    /// model still sees true context) but **never** executes a side-effecting tool —
    /// edits, file creation, and shell/verification commands are short-circuited to
    /// a `[dry-run]` note instead of running (spec 06 `--dry-run`). The workspace is
    /// left untouched.
    pub dry_run: bool,
    /// Emit the fully-assembled prompt each turn as an [`AgentEvent::PromptAssembled`]
    /// — *what the model actually saw* (spec 06 `--verbose`, spec 05). Off by
    /// default because the payload is large; renderers/logs only get it when asked.
    pub verbose: bool,
    /// Optional human confirmer for confirm-gated shell commands (spec 04 / spec 06).
    /// When `None`, an unapproved `run_command` is auto-denied exactly as before
    /// (headless). When set, the loop blocks and asks before denying — the seam the
    /// GUI's approve/deny buttons and the CLI's interactive prompt drive. `Arc` keeps
    /// `AgentConfig: Clone` and lets the handle cross to the worker thread.
    pub confirmer: Option<Arc<dyn Confirmer>>,
    /// Where `run_verification` runs (spec 12): the host, or a per-run Docker container.
    /// Docker gives generated code a pinned toolkit + a known layout so the tests run
    /// against a reproducible env (the GUI defaults to it). Defaults to the host.
    pub sandbox: sc_verify::Sandbox,
    /// On a test-failure stall, run a root-cause diagnosis (a focused debugger pass over the
    /// FULL test output + all source files) and inject it, instead of the generic
    /// self-recovery directive (spec 03 — recovery). The model debugs blind otherwise: it
    /// reacts to a downstream symptom and edits the wrong file. Default OFF — it costs an
    /// extra suite run + model call per stall, so it ships dark and is enabled once proven
    /// on the ladder. Bounded by `DIAGNOSIS_LIMIT` and gated on a configured verify command.
    pub diagnose: bool,
    /// Stream each turn's generation, emitting [`AgentEvent::ContentDelta`] per token so a UI
    /// can show the reply — including a file edit being written — appear live, word by word.
    /// Off by default (the blocking `generate` path); the GUI's iterate/fix runs turn it on.
    pub stream: bool,
    /// Cooperative cancellation: when set and flipped to `true`, the loop stops at the next
    /// turn boundary with `StopReason::Cancelled` (it can't interrupt an in-flight model call,
    /// but won't start another). The GUI's Cancel button flips this. `Arc` keeps
    /// `AgentConfig: Clone` and lets the flag cross to the worker thread.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("max_steps", &self.max_steps)
            .field(
                "effective_context_fraction",
                &self.effective_context_fraction,
            )
            .field("response_reserve_tokens", &self.response_reserve_tokens)
            .field("observation_line_cap", &self.observation_line_cap)
            .field("read_file_line_cap", &self.read_file_line_cap)
            .field("keep_recent_turns", &self.keep_recent_turns)
            .field("repo_map_top_k", &self.repo_map_top_k)
            .field("permission", &self.permission)
            .field("verify_command", &self.verify_command)
            .field("plan_first", &self.plan_first)
            .field("repeat_limit", &self.repeat_limit)
            .field("no_progress_limit", &self.no_progress_limit)
            .field("step_retry_budget", &self.step_retry_budget)
            .field("system_suffix", &self.system_suffix)
            .field("focus_files", &self.focus_files)
            .field("dry_run", &self.dry_run)
            .field("verbose", &self.verbose)
            // `dyn Confirmer` isn't `Debug`; report presence only.
            .field("confirmer", &self.confirmer.is_some())
            .field("sandbox", &self.sandbox)
            .field("diagnose", &self.diagnose)
            .finish()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            effective_context_fraction: 0.75,
            response_reserve_tokens: 1024,
            observation_line_cap: 40,
            read_file_line_cap: 400,
            keep_recent_turns: 3,
            repo_map_top_k: 30,
            permission: PermissionPolicy::default(),
            verify_command: None,
            plan_first: false,
            repeat_limit: 3,
            no_progress_limit: 4,
            step_retry_budget: 3,
            system_suffix: None,
            focus_files: Vec::new(),
            dry_run: false,
            verbose: false,
            confirmer: None,
            sandbox: sc_verify::Sandbox::default(),
            diagnose: false,
            stream: false,
            cancel: None,
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
    /// Why the run stopped (spec 06 — honest stop lines). `finished` is a
    /// convenience alias for `stop_reason == Finished`.
    pub stop_reason: StopReason,
    /// How many times the harness intervened (re-plan / advisor nudge) to recover
    /// the agent from a stall (spec 03).
    pub interventions: usize,
}

pub(super) const TASK_PREFIX: &str = "You are a coding agent working in a project directory. \
Make the failing test pass. Follow this loop: \
1) read_file the file you need to change (don't just search repeatedly); \
2) edit_file it with a precise change; \
3) run_verification to run the tests (use run_verification, NOT run_command — \
shell is blocked); read which tests still fail and fix them; \
4) finish only when the tests pass. \
Take a concrete action every turn — prefer editing over searching.\n\n";

/// The same loop, for a run where the shell IS allowed.
///
/// The difference is step 1. Telling a model to read first traps it in a read loop —
/// the same trap [`FOCUS_TASK_PREFIX`] was written to avoid. With a shell available the
/// better first move is to *run something*: a command's output turns a symptom into a
/// concrete, located fact, which is what a model needs before it can commit to an edit.
///
/// `TASK_PREFIX` must not be used when shell is permitted: it states "shell is blocked",
/// and a model that is told a tool is unavailable will not reach for it however the
/// permission policy is configured.
pub(super) const TASK_PREFIX_SHELL: &str = "You are a coding agent working in a project \
directory. Make the failing test pass. Follow this loop: \
1) run_command to investigate — grep for the symbol, run the failing test, print a value. \
Prefer running something over reading a whole file; \
2) edit_file the source with a precise change once you know what is wrong; \
3) run_verification to run the tests; read which tests still fail and fix them; \
4) finish only when the tests pass. \
Take a concrete action every turn. Do not read the same file twice — if you have read it, \
you have it.\n\n";

/// System preamble for a focus-scoped run: the file you must edit is already shown
/// to you every turn, so don't read it — edit it. Used by the swarm worker (and
/// any caller that sets `focus_files`).
pub(super) const FOCUS_TASK_PREFIX: &str =
    "You fix code. The file you must change is shown below IN FULL, \
between === markers — it updates after each edit, so never read it again. The files it imports \
from are also shown in full as READ-ONLY context (between --- markers); any remaining files \
appear as a signature map (`path:line  name`). You already have everything you need — do NOT \
read_file any of these. Each turn, do ONE of:\n\
- edit_file / write_file: change the shown file. Copy old_str exactly from it.\n\
- run_verification: run the tests to see what still fails.\n\
- finish: stop, once the tests pass.\n\
Edit the shown file (using the imported files and the map for context), verify, repeat.\n\n";
