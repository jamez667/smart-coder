//! What the loop is configured with ([`AgentConfig`]) and what it reports
//! ([`AgentReport`]), plus the two task-prefix system prompts.

use std::sync::Arc;

use sc_tools::{PermissionPolicy, ToolRegistry};

use crate::confirm::Confirmer;
use crate::event::FaultKind;
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
    /// The MINIMUM number of most-recent turns kept verbatim (spec 05). This is a floor,
    /// not a cap: the recent window grows freely while the prompt fits the budget, and only
    /// when it does not fit does the loop evict the OLDEST whole turn (the action, its
    /// observation, and any harness note attached to it) into the rolling history summary,
    /// one at a time, never below this many. Appending rather than trimming keeps the prompt
    /// prefix byte-stable between turns so the backend's KV cache is reused.
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
    /// An extra tool surface supplied by the caller, consulted before the loop's
    /// own routing (spec 04 — the harness decides what a tool is).
    ///
    /// `None` in every shipping path, so a normal run is unaffected. It exists so
    /// an experiment can offer the model a tool this crate has no dependency on —
    /// the `sc-gateway` A/B needs the agent to CALL a gateway, and wiring that in
    /// directly would ship the thing being measured.
    pub external_tool: Option<Arc<dyn crate::agent::dispatch::ExternalTool>>,
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
            .field("external_tool", &self.external_tool.is_some())
            .field("sandbox", &self.sandbox)
            .field("diagnose", &self.diagnose)
            .finish()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        // MEASURED, not guessed. These were the "toy task" defaults, and each was
        // proven wrong against real work -- but only the eval path was ever fixed, so
        // `sc-cli run` (the production path) kept shipping values the evals had
        // already shown to fail. Raising them here means every caller inherits what
        // was measured; a caller that genuinely wants less can still say so.
        Self {
            // Pooled across runs, solves landed at step 33, 35 and 48. A 25-step cap
            // discards those and reports them as failures.
            max_steps: 40,
            // The fraction of the window the PROMPT may fill. It sat at 0.75 while
            // the only counter was a char-based estimator that undercounted llama.cpp
            // by 23%: the missing quarter was headroom for the estimator, not for the
            // model. Now the backend's own tokenizer answers wherever llama.cpp serves
            // one, and the per-message template cost is charged explicitly, so the
            // margin no longer needs to cover an unknown. What stays reserved is for
            // the reply (`response_reserve_tokens` below), and a small margin for the
            // template's remaining unknowns and for the native `tools` JSON on a
            // backend that has not told the builder its size.
            effective_context_fraction: 0.9,
            // A reasoning model spends tokens before it emits the call, and a
            // truncated turn yields NO call -- indistinguishable from declining to
            // act. At 1024, one edit request in five returned nothing at all.
            //
            // Sized from what was measured, not guessed: across a full ten-rung run
            // Tiel's largest reply was 1,328 tokens (see `AgentReport::peak_reply_tokens`,
            // which exists so this number can be checked). 2048 is ~1.5x that peak;
            // the previous 6144 was subtracted from EVERY prompt of every turn to
            // hold room that was never used. A reasoning model that spends more
            // before it answers -- a rambling model on the same suite hit 14,202 --
            // must raise this, and the run report says when it should: watch
            // `peak_reply_tokens` and the `ReplyTruncated` harness fault.
            response_reserve_tokens: 2048,
            // 40 lines amputates a test traceback exactly where the assertion is.
            observation_line_cap: 200,
            // The model must read the failing test; clipping it mid-file is the
            // harness hiding the answer.
            read_file_line_cap: 800,
            // 3 turns cannot hold a multi-file task: on a four-file task the first
            // file is compacted to a summary before the fourth is read, so the model
            // re-reads it forever. Measured on one rung: 87 turns -> 24, reads 59 ->
            // 13, stalls 3 -> 0, with the EDIT COUNT UNCHANGED at 11 -- every extra
            // read was the harness discarding what it had just handed over.
            keep_recent_turns: 10,
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
            external_tool: None,
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
    /// Prompt tokens summed over EVERY turn of the run.
    ///
    /// The peak answers "did a prompt fit?"; this answers "what did the task
    /// cost?" — and they diverge exactly where it matters. A run that reads the
    /// right file once and a run that re-reads four files six times can share a
    /// peak and differ several-fold here, which is the whole premise behind
    /// giving a small model less to look at.
    pub total_prompt_tokens: usize,
    /// Of `total_prompt_tokens`, how many the backend served from its KV cache
    /// instead of re-evaluating -- summed over every turn that reported it.
    ///
    /// **`total_prompt_tokens` cannot see the append-only prompt work.** It
    /// counts what the harness sent, and a stable prefix sends exactly as many
    /// tokens as a shifting one; what changes is how many the server has to
    /// re-prefill. This pair is that measurement: against
    /// `total_prefilled_prompt_tokens`, a healthy run re-prefills only the newly
    /// appended tokens each turn and this dominates.
    ///
    /// `0` when the backend never reported a split, which is indistinguishable
    /// here from a genuine zero -- the per-turn `ModelTurn` events keep the
    /// `Option` if a caller needs to tell them apart.
    pub total_cached_prompt_tokens: usize,
    /// Of `total_prompt_tokens`, how many the backend actually PREFILLED --
    /// summed over every turn that reported it. The compute the run really cost.
    pub total_prefilled_prompt_tokens: usize,
    /// The largest REPLY the model produced, in tokens.
    ///
    /// Reported so `response_reserve_tokens` can be checked against reality rather
    /// than guessed. The reserve is subtracted from the prompt budget every turn, so
    /// an over-generous one silently costs context on every single request --
    /// measured, Tiel's largest reply across a full ten-rung run was 1,328 tokens
    /// against a 12,288 reserve, while a rambling model on the same suite hit
    /// 14,202. It is a per-MODEL number and nobody could see it.
    pub peak_reply_tokens: usize,
    /// Harness faults raised during the run, by kind.
    ///
    /// **A run that degraded its own input must not look like a clean one.** These
    /// were emitted to the event stream from the start and never counted here, so
    /// the only way to see them was to parse an NDJSON log by hand -- a run with
    /// sixteen truncated replies printed exactly like a run with none. Counting
    /// them means the report can say so.
    pub harness_faults: Vec<(FaultKind, usize)>,
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

/// The build loop, with the reading and editing tools named from the registry.
///
/// [`TASK_PREFIX`] hardcodes `read_file`. A registry offering a different reading
/// tool gets a prompt naming one it does not have, and a model does what it is
/// told — the same failure this file already documents for
/// [`INVESTIGATE_TASK_PREFIX`], where a build prompt over a read-only registry
/// sent a model hunting for `edit_file` and dumping prose for four turns.
///
/// Measured here too: an experimental registry offering `ask` in place of
/// `read_file`/`run_command` still received a prompt naming both, and spent
/// roughly two extra turns per task looking for tools it did not have — a
/// uniform ~5,000-token overhead on tasks whose entire fixture is 1.4 KB.
///
/// The tool names in a prompt are part of the tool contract, so they come from
/// the registry like everything else about it.
pub(super) fn task_prefix_for(registry: &ToolRegistry) -> String {
    let pick = |candidates: &[&'static str], fallback: &'static str| -> &'static str {
        candidates
            .iter()
            .copied()
            .find(|t| registry.get(t).is_some())
            .unwrap_or(fallback)
    };
    let read = pick(
        &["read_file", "ask", "read_function", "search_code"],
        "read_file",
    );
    let edit = pick(&["edit_file", "write_file"], "edit_file");
    format!(
        "You are a coding agent working in a project directory. \
         Make the failing test pass. Follow this loop: \
         1) {read} the file you need to change (don't just search repeatedly); \
         2) {edit} it with a precise change; \
         3) run_verification to run the tests (use run_verification, NOT run_command -- \
         shell is blocked); read which tests still fail and fix them; \
         4) finish only when the tests pass. \
         Take a concrete action every turn -- prefer editing over searching.\n\n"
    )
}

/// The loop for a READ-ONLY run: answer a question about the code, change nothing.
///
/// `TASK_PREFIX` is a *build* prompt — "make the failing test pass", "edit_file it",
/// "run_verification", "finish only when the tests pass". Handed to a read-only registry it
/// describes three tools that are not there, and a model does what it is told: measured on a
/// live run, it hunted for `edit_file`, could not find it, and dumped ~45,000 characters of
/// prose four turns running, each one truncated at the reply cap and rejected as "no JSON
/// tool object". The answer it had already worked out was thrown away.
///
/// The difference that matters is what `finish` MEANS. In a build run finishing is a signal
/// and its argument is decoration; here the argument IS the deliverable, and a `finish` with
/// an empty argument is the run failing while reporting success.
pub(super) const INVESTIGATE_TASK_PREFIX: &str =
    "You are a code investigator working in a project directory. Answer the user's question by READING the code. You have read-only tools: there is no way to edit files, run commands or run tests here, so do not look for one and do not describe an edit as though you were making it.

Follow this loop: 1) read_file / read_function / search_code to find the relevant code; 2) when you know the answer, call finish and put the WHOLE ANSWER in its `summary` argument.

The `summary` argument is the ONLY thing the user sees — prose written outside a JSON tool call is discarded. Name the file and the line, explain the cause, and give the exact change that would fix it. Take a concrete action every turn.

";

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
