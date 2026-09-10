//! Tool execution and its surrounding classifiers: the permission/dry-run gate + routing
//! for a single validated call, the `finish` whole-suite gate, the batched-write pre-apply,
//! and the small predicates the loop uses to decide how to treat a call or its observation.

use std::path::Path;

use sc_tools::{execute, Journal, PermissionPolicy, ToolOutcome, ToolRegistry};

use crate::confirm::{Confirmation, Confirmer};
use crate::event::{AgentEvent, EventSink};
use crate::text::first_line;

use super::AgentConfig;

/// A tool this crate does not know how to execute, supplied by the caller.
///
/// The loop consults this **before** its own routing, for exactly one purpose:
/// to let an experiment offer a tool surface `sc-core` has no dependency on.
/// The gateway experiment (`sc-gateway`) is the motivating case — measuring
/// whether one classified `ask` beats a menu of six needs the agent to be able
/// to CALL it, and wiring that into this crate directly would ship the thing
/// being measured.
///
/// Defaulted to `None` everywhere, so a run that supplies nothing behaves
/// byte-for-byte as before. Returning `None` from [`execute`] falls through to
/// the loop's normal routing, so an implementor handles only what it claims.
///
/// [`execute`]: ExternalTool::execute
///
/// `Send + Sync` for the same reason as [`Confirmer`](crate::Confirmer): the agent
/// runs on a worker thread and the config is moved into it.
pub trait ExternalTool: Send + Sync {
    /// Handle `call`, or return `None` to let the loop route it as usual.
    fn execute(&self, call: &sc_tools::ValidatedCall, workspace: &Path) -> Option<ToolOutcome>;
}

/// Is the last recorded verification still a valid answer for the workspace as it stands?
///
/// MEASURED WASTE. On a real 15-turn refactor (117s end to end) the verify command ran FIVE
/// times at ~6s each -- the run-start baseline, two auto-verifies after edits, one the model
/// asked for itself, and the finish gate. Several were redundant: nothing had written to the
/// workspace between them. `cargo check` on this workspace is 6s; a project with a real test
/// suite pays minutes for the same nothing.
///
/// The rule is simply: **never run the verify command when the answer cannot have changed.**
/// The workspace only changes when something writes to it, and the loop already tracks that.
/// So this is a one-bit cache invalidation flag, not a cache: the ANSWER lives in the run log
/// ([`crate::runlog::RunLog::last_verification_green`] and `last_verification`), which is the
/// same place every stop report reads it from. This only says whether that answer is stale.
///
/// LIFECYCLE:
/// - starts dirty (nothing has been verified yet, so there is no answer to reuse);
/// - [`Self::verified`] marks it clean, immediately after any verification runs;
/// - [`Self::touched`] marks it dirty again, from every turn that could have written to the
///   workspace.
///
/// WHEN IN DOUBT, DIRTY. A redundant verification costs seconds; a skipped necessary one
/// reports a wrong answer as verified.
#[derive(Debug, Clone, Copy)]
pub(super) struct VerifyFreshness {
    dirty: bool,
}

impl Default for VerifyFreshness {
    fn default() -> Self {
        // Dirty at construction: nothing has been verified this run, so there is nothing to
        // reuse and the first verification must actually run. (The run-start baseline is NOT
        // skippable for the same reason -- it establishes the shape of the run.)
        Self { dirty: true }
    }
}

impl VerifyFreshness {
    /// A verification just ran, so the recorded answer describes the workspace as it is now.
    pub(super) fn verified(&mut self) {
        self.dirty = false;
    }

    /// Something may have written to the workspace. Called with the loop's own `changed`
    /// signal OR'd with every other way a turn can write: the batched-write pre-apply, and
    /// any `run_command` (a shell command can `sed -i` anything, so it counts as a write
    /// whether or not it was one -- the same conservative treatment
    /// `stable.refresh_if_changed` already gives it).
    pub(super) fn touched(&mut self, changed: bool) {
        self.dirty |= changed;
    }

    /// May a caller reuse `last_green` -- the run log's last recorded verification outcome --
    /// instead of running the command? Only when nothing has changed since it was recorded
    /// AND it was green. A red result is never reused: the model is expected to act on it,
    /// and re-running is how it learns whether its fix landed.
    pub(super) fn reusable_green(&self, last_green: Option<bool>) -> bool {
        !self.dirty && last_green == Some(true)
    }
}

/// Outcome of the whole-suite gate at `finish`.
pub(super) enum FinishGate {
    /// Finish is honored; the bool is the verified state (None → no verify cmd).
    Allow(Option<bool>),
    /// Finish is refused with an observation the model must react to.
    Refuse(String),
}

/// Run the configured verification before honoring `finish` (spec 11). With no
/// command configured, finish is always allowed (verified = None).
///
/// `fresh_green` is the loop's [`super::VerifyFreshness`] answer to "is the last recorded
/// verification still valid, and was it green?". When it is `true` the suite is re-run for
/// NOTHING: nothing has touched the workspace since, so the answer cannot have moved. On a
/// measured 15-turn refactor this gate was the fifth ~6s verification of a 117s run.
///
/// A STALE green must never be trusted -- that is the whole point of the flag. Every turn
/// that could have touched the workspace dirties it (see [`super::VerifyFreshness`]), and
/// only an undirtied green is honored here. A red result never short-circuits, so a skip
/// can never turn a red suite into a reported green.
///
/// ASSUMPTION: the harness is the only writer to the workspace during a run. A human
/// editing files by hand, or a background build, is not observed. The loop already assumes
/// this elsewhere (the retrieval cache's `refresh_if_changed`, the journal's snapshots).
/// Because being wrong here means reporting an unverified workspace as verified, the flag
/// is dirtied generously: a redundant verification costs seconds, a wrongly skipped one
/// ships a false green.
pub(super) fn gate_finish(
    sandbox: &sc_verify::Sandbox,
    verify_command: &Option<String>,
    workspace: &Path,
    fresh_green: bool,
) -> FinishGate {
    match verify_command {
        None => FinishGate::Allow(None),
        // Fresh green: the recorded answer still stands, so honour it without a re-run.
        Some(_) if fresh_green => FinishGate::Allow(Some(true)),
        Some(cmd) => {
            let report = sc_verify::run_verification_in(sandbox, workspace, cmd);
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
/// `run_command`/`run_verification` to sc-verify (neither belongs in the pure-fs
/// tool registry); everything else is the registry's `execute`.
// Each parameter is a distinct, irreducible concern of one tool dispatch (the call,
// the registry/policy it's checked against, the confirm seam + its session allowlist,
// the verify command, the dry-run flag, the workspace); bundling them into a struct
// would only move the noise. Private routing fn — keep it flat.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch(
    call: &sc_tools::ValidatedCall,
    registry: &ToolRegistry,
    policy: &PermissionPolicy,
    confirmer: Option<&dyn Confirmer>,
    session_allow: &mut Vec<String>,
    sandbox: &sc_verify::Sandbox,
    verify_command: &Option<String>,
    dry_run: bool,
    workspace: &Path,
    external: Option<&dyn ExternalTool>,
) -> ToolOutcome {
    // Permission gate — the harness decides, outside the model's control (spec 04).
    if let Some(spec) = registry.get(&call.name) {
        if let sc_tools::Decision::Deny(reason) = policy.check(call, spec.side_effect) {
            // Only `run_command` is confirm-gated. Other denials (frozen tests, etc.)
            // keep their current auto-deny behavior untouched.
            if call.name == "run_command" {
                let cmd = call.str("command").unwrap_or_default();

                // A command approved-and-remembered earlier this run is already
                // allowed — fall through to execution without re-prompting.
                let remembered = session_allow.iter().any(|p| cmd.starts_with(p.as_str()));
                if !remembered {
                    // A small model often reaches for `run_command "pytest"/"cargo
                    // test"`; redirect it to the allowed run_verification tool instead
                    // of prompting or denying (spec 04 — structured feedback). This
                    // takes precedence over the confirmer.
                    if looks_like_test_command(call.str("command")) {
                        return ToolOutcome::Observation(
                            "run_command denied (shell is blocked). To run the tests, use \
                             {\"tool\":\"run_verification\"} instead."
                                .to_string(),
                        );
                    }
                    // Ask the human, iff a confirmer is wired. No confirmer ⇒ today's
                    // exact behavior: the static Deny stands.
                    match confirmer {
                        None => {
                            return ToolOutcome::Observation(format!(
                                "{} denied: {reason}",
                                call.name
                            ))
                        }
                        Some(c) => match c.confirm_command(cmd, &reason) {
                            Confirmation::Deny(why) => {
                                return ToolOutcome::Observation(format!(
                                    "run_command denied: {why}"
                                ))
                            }
                            Confirmation::AllowRemember { prefix } => session_allow.push(prefix),
                            Confirmation::AllowOnce => {}
                        },
                    }
                }
                // Approved (once, remembered, or matched a remembered prefix): fall
                // through to the shared dry-run check + execution below, so `--dry-run`
                // is still honored for a human-approved command.
            } else {
                return ToolOutcome::Observation(format!("{} denied: {reason}", call.name));
            }
        }

        // Dry-run (spec 06): preview only. Read-only tools still run for real (the
        // model needs true context to reason); any side-effecting tool — edits,
        // create_file, run_command, run_verification — is short-circuited to a note
        // so the workspace is never touched and no process is spawned.
        if dry_run && spec.side_effect != sc_tools::SideEffect::ReadOnly {
            let arg = key_arg(call);
            let target = if arg.is_empty() {
                String::new()
            } else {
                format!(" {arg}")
            };
            return ToolOutcome::Observation(format!(
                "[dry-run] would {}{target}; no changes written",
                call.name
            ));
        }
    }

    // The caller's own tools first (spec 04 — the harness decides what a tool is).
    // Checked after the permission and dry-run gates above, so an external tool is
    // no less governed than a built-in one.
    if let Some(ext) = external {
        if let Some(outcome) = ext.execute(call, workspace) {
            return outcome;
        }
    }

    match call.name.as_str() {
        "find_symbol" => {
            let name = call.str("name").unwrap_or_default();
            ToolOutcome::Observation(sc_index::find_symbol(workspace, name))
        }
        "run_command" => {
            // Drop a leading `cd <somewhere> &&`. The command already runs with the
            // workspace as its cwd, so the cd is redundant at best -- and on Windows
            // the absolute path it names is full of backslashes that `sh -c` eats as
            // escapes, so the cd FAILS, and a model that now believes it is lost
            // starts inventing directories: six consecutive `cd /c/Users/mail/
            // Projects/...` attempts at a path that never existed, in one measured
            // run. Removing the class of error beats asking the model not to make it.
            let cmd = sc_verify::strip_leading_cd(call.str("command").unwrap_or_default());
            // Honour the configured sandbox, exactly as `run_verification` does below.
            // This called the Host-pinned wrapper, so a shell command ran on the host
            // while the tests it was investigating ran in the container — a different
            // OS, a different filesystem, and none of the project's dependencies.
            let r = sc_verify::run_command_in(sandbox, workspace, cmd);
            ToolOutcome::Observation(format!(
                "run_command {cmd:?} exited {}:\n{}",
                r.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                r.output.trim()
            ))
        }
        "run_verification" => match verify_command {
            Some(cmd) => ToolOutcome::Observation(
                sc_verify::run_verification_in(sandbox, workspace, cmd).observation(),
            ),
            None => ToolOutcome::Observation(
                "run_verification: no verification command is configured for this project".into(),
            ),
        },
        _ => execute(call, workspace),
    }
}

/// Pre-apply the EXTRA writes of a batched turn (thread 3): the leading run of distinct-path
/// `create_file`/`write_file` calls beyond the first, which `extract_write_batch` has vetted
/// as safe to apply in sequence (different files, no observe→react needed between them). The
/// FIRST call is left for the normal dispatch; this applies calls 2..N directly, journals
/// each, emits ToolCall/ToolResult events for them, and returns a short note to prepend to the
/// turn's observation so the model sees all the writes happened. Honors the permission gate
/// (a frozen path is skipped). Returns "" when there's nothing extra to apply.
pub(super) fn pre_apply_batched_writes(
    raw: &str,
    registry: &ToolRegistry,
    policy: &PermissionPolicy,
    workspace: &Path,
    journal: &mut Journal,
    sink: &dyn EventSink,
) -> String {
    let batch = crate::strategy::extract_write_batch(raw, registry);
    // batch[0] is the first call (handled by the normal dispatch); apply 2..N here.
    if batch.len() < 2 {
        return String::new();
    }
    let mut applied: Vec<String> = Vec::new();
    for call in batch.iter().skip(1) {
        let Some(path) = call.str("path").map(str::to_string) else {
            continue;
        };
        // Respect the permission gate (e.g. frozen test files are never written).
        if let Some(spec) = registry.get(&call.name) {
            if matches!(
                policy.check(call, spec.side_effect),
                sc_tools::Decision::Deny(_)
            ) {
                continue;
            }
        }
        let before = Journal::snapshot(workspace, &path);
        let outcome = execute(call, workspace);
        let after = Journal::snapshot(workspace, &path);
        if before != after {
            journal.record(workspace, &path, before);
            applied.push(path.clone());
        }
        let summary = match &outcome {
            ToolOutcome::Observation(o) => first_line(o),
            ToolOutcome::Finished => "finished".to_string(),
        };
        sink.record(&AgentEvent::ToolCall {
            tool: call.name.clone(),
            arg: path.clone(),
        });
        sink.record(&AgentEvent::ToolResult {
            summary: summary.clone(),
            full: summary,
            is_error: false,
        });
    }
    if applied.is_empty() {
        String::new()
    } else {
        format!(
            "(harness also applied {} more batched file write(s) from this turn: {})\n",
            applied.len(),
            applied.join(", ")
        )
    }
}

/// If `call` is a mutating, path-bearing tool, return its workspace-relative
/// path (so the journal can snapshot it). `run_verification`/`run_command` are
/// mutating-ish but have no single file to record.
pub(super) fn mutating_path(
    call: &sc_tools::ValidatedCall,
    registry: &ToolRegistry,
) -> Option<String> {
    let spec = registry.get(&call.name)?;
    if spec.side_effect != sc_tools::SideEffect::Mutating {
        return None;
    }
    call.str("path").map(|s| s.to_string())
}

/// The line cap to truncate a tool's observation to before it re-enters context. A
/// `read_file` returns source the model must edit, so it gets the generous
/// `read_file_line_cap` (whole small/medium files); a runaway command/test log gets the
/// tight `observation_line_cap` where error-first truncation keeps the signal (spec 05).
pub(super) fn observation_cap_for(tool: &str, cfg: &AgentConfig) -> usize {
    match tool {
        // A file read is source the model edits; a verification report is failure-first and
        // carries the underlying exception the model must see — both need real room. A
        // runaway command/test log keeps the tight default where error-first truncation
        // does the work.
        // `ask` returns whatever capability answered — usually a file — so it
        // gets the same generous cap. Capping it at 40 lines truncated whole-file
        // answers mid-source, which is the harness hiding the very code the model
        // asked for, and it re-asks.
        // `read_function` is a file read too, and it takes the same paged path, so it
        // gets the same generous cap. It used to draw the 200-line command cap: a
        // function longer than that was cut and had to be paged through, which is the
        // harness rationing source code the model asked for BY NAME. Rare (a "giant"
        // function is 120 lines) but pure loss when it happens.
        "read_file" | "read_function" | "run_verification" | "ask" => cfg.read_file_line_cap,
        _ => cfg.observation_line_cap,
    }
}

/// Did `truncate_observation` cut `obs` down to `trimmed` BLIND -- a head/tail slice
/// with no error line to anchor on -- and hide more than it showed? Returns
/// `(total_lines, lines_shown)` when so, `None` for an uncut result, an error-first
/// cut (which kept the signal by construction), or a cut that still showed the
/// model at least half.
///
/// The paths are told apart by the marker the truncator leaves. Only the blind slice
/// ([`sc_context::truncate_observation`]'s head/tail fallback) writes
/// `… [N line(s) truncated] …`; the error-first path writes `… [N line(s) skipped] …`
/// between the lines it kept, and the paged-read path
/// ([`sc_context::truncate_paged_read`]) writes a `pass start=N for the next page` /
/// `trailing line(s) not shown` note. Only the marker lines are discounted from the
/// shown count.
///
/// A cut PAGED READ is deliberately not a blind cut, however much it dropped. Its
/// kept region is a contiguous prefix — nothing went missing from inside what the
/// model can see — and the note names the exact `start` that fetches the rest, so it
/// is an ordinary, recoverable page boundary rather than evidence the model never
/// knew it was missing. Raising a fault on every page turn of a large file would be
/// noise, and would push the operator to raise a cap that is working as intended.
pub(super) fn blind_cut(obs: &str, trimmed: &str) -> Option<(usize, usize)> {
    let total = obs.lines().count();
    let mut shown = 0usize;
    let mut markers = 0usize;
    for l in trimmed.lines() {
        if l.contains("line(s) skipped]") {
            return None;
        }
        if l.contains("line(s) truncated]") {
            markers += 1;
        } else {
            shown += 1;
        }
    }
    if markers == 0 || shown >= total {
        return None;
    }
    let dropped = total - shown;
    (dropped > shown).then_some((total, shown))
}

/// The key argument of a call, for the repeat-dedup history record (path or
/// query/name). For a windowed `read_file` the window (`start`/`limit`) is folded
/// into the key so paging THROUGH a file — `read_file(a.rs, start=1)` then
/// `read_file(a.rs, start=51)` — reads as two DISTINCT actions, not a refused
/// "duplicate". Without this, any file past the first window is unreachable: the
/// second page hashes identical to the first and gets nudged away, so the model
/// can never see lines 51+ of a file it must edit. A bare re-read (same path, no
/// window, or the identical window) still dedups, which is the case we want to nudge.
///
/// **This value is a PATH wherever a path-bearing tool produced it**, and callers rely on
/// that: the loop threads it into [`AgentEvent::ToolCall`]'s `arg` for display, tracks the
/// write-loop breaker's streak by it, hands it to `rewrite_target` to READ the file off
/// disk, and inlines it into the directive naming the file the model must rewrite. So
/// nothing may be appended to it here. The stall detector's extra discriminator lives in
/// [`action_key`], which is used for the action hash alone.
pub(super) fn key_arg(call: &sc_tools::ValidatedCall) -> String {
    // `run_command`'s parameter is `command`, which was not in the list below — so
    // every shell call hashed to the SAME empty key. Two genuinely different
    // commands looked like a repeat (a false stall), and a model looping on one
    // command looked no different from one making progress.
    //
    // Normalized through `strip_leading_cd` for the same reason the dispatch does
    // it: `cd /a && ls` and `cd /b && ls` both execute as plain `ls`, so they are
    // the same action and must hash alike, or the detector misses a real loop.
    if let Some(cmd) = call.str("command") {
        return sc_verify::strip_leading_cd(cmd).trim().to_string();
    }
    // `finish`'s `summary` is the ANSWER on a read-only run, and it was not in the list
    // below -- the same omission this function's own comment describes for `command`, with
    // a worse consequence: the model's whole reply reached `ToolCall.arg` as an empty
    // string and the run reported success having returned nothing.
    if let Some(summary) = call.str("summary") {
        return summary.to_string();
    }
    // `crate` joins the list for the reason the two comments above describe: it is
    // `cargo_info`'s only parameter, and omitting it would hash every call about
    // every crate to the same empty key -- asking about `sc-proto` and then
    // `sc-core` would read as a repeat, which is the third instance of this bug.
    // `need` is a gateway `ask`'s question, and omitting it is the FOURTH
    // instance of the bug the three comments above describe: every ask would
    // hash to the same empty key, so asking about one file and then another
    // read as a repeat and got nudged away as a false stall.
    // `need` FIRST: an `ask` carries both `need` and an optional `path`, and
    // keying on the path would make two different questions about the same file
    // hash identically -- a false repeat, which is what this whole function
    // exists to avoid.
    for k in ["need", "path", "query", "name", "crate"] {
        if let Some(v) = call.str(k) {
            let start = call.int("start");
            let limit = call.int("limit");
            return match (start, limit) {
                (None, None) => v.to_string(),
                _ => format!(
                    "{v}@{}:{}",
                    start.map(|n| n.to_string()).unwrap_or_default(),
                    limit.map(|n| n.to_string()).unwrap_or_default()
                ),
            };
        }
    }
    String::new()
}

/// The identity of a call for the **stall detector** — [`key_arg`] plus, for an anchored
/// or named edit, the anchor that says WHERE in the file the edit lands.
///
/// **An EDIT's identity is its path AND its anchor.** Keying an edit on its path alone was
/// the fifth — and worst — instance of the bug [`key_arg`]'s own comments describe four
/// times over (for `command`, `summary`, `crate` and `need`). Every editor keyed on `path`,
/// so three consecutive, entirely different, entirely successful `edit_file` calls on one
/// file hashed to one action and tripped `repeat_limit` (3). Measured on `rust-symptomatic`:
/// three edits that each applied cleanly — the harness's own observations that turn read
/// `edit_file lib.rs ok (1 replacement)` and `now passing: a_full_window_evicts_the_oldest`
/// — were answered with "STOP — you are stuck in a loop calling 'edit_file' and making no
/// progress". The directive then banned `edit_file`, the model rewrote the file wholesale,
/// and the run regressed from 1 failing test to 2. The false positive landed on the one
/// tool that makes progress, which is the worst place for it.
///
/// SEPARATE FROM [`key_arg`] on purpose. That value is a real path, and the loop reads the
/// file off disk with it, tracks the write-loop breaker's streak by it, and names it to the
/// model in the rewrite directive — appending an anchor there made the harness order a
/// `write_file` to a path called `big.rs#249d985d`. Only the hash needs the discriminator,
/// so only the hash gets it.
pub(super) fn action_key(call: &sc_tools::ValidatedCall) -> String {
    let base = key_arg(call);
    match edit_anchor(call) {
        // `path#xxxxxxxx` — the path stays in the clear so a debug dump still reads.
        Some(anchor) => format!("{base}#{:08x}", short_hash(&anchor)),
        None => base,
    }
}

/// What distinguishes one edit of a file from another edit of the SAME file — the thing
/// [`action_key`] folds into the hash so the stall detector can tell them apart. `None` for
/// any call that is not an anchored/named edit, which keeps every other tool's action
/// identity byte-for-byte what it was.
///
/// Per tool, the anchor is the argument that says WHERE in the file the edit lands:
/// - `edit_file` → `old_str`, the exact snippet being replaced;
/// - `edit_lines` → `start:end`, the whole line range (`start` alone is not enough:
///   `edit_lines(a.rs, 1..5)` and `edit_lines(a.rs, 1..9)` are different edits);
/// - `edit_function` → `name`, the function being replaced.
///
/// Deliberately NOT the replacement text (`new_str`/`new_text`/`new_body`/`content`).
/// Writing two different bodies over the SAME anchor is thrash, and must still read as a
/// repeat; and a whole-file `write_file`/`create_file` has no anchor at all, so rewriting
/// one file three times running keeps tripping the detector exactly as before.
fn edit_anchor(call: &sc_tools::ValidatedCall) -> Option<String> {
    match call.name.as_str() {
        "edit_file" => call.str("old_str").map(str::to_string),
        "edit_lines" => Some(format!(
            "{}:{}",
            call.int("start").unwrap_or_default(),
            call.int("end").unwrap_or_default()
        )),
        "edit_function" => call.str("name").map(str::to_string),
        _ => None,
    }
}

/// An 8-hex-digit digest of an edit anchor, for [`action_key`]'s `path#xxxxxxxx`.
///
/// Hashed rather than inlined because an `old_str` is arbitrary source — multi-line, and
/// routinely kilobytes. A fixed-width suffix keeps the key one short readable line
/// (`impl.sh#3f2a1c7b`) however big the anchor is. The suffix only has to DIFFER, not be
/// legible: a collision costs one false repeat, which is what the action hash already
/// tolerates by construction.
fn short_hash(s: &str) -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as u32
}

/// Does a shell command look like an attempt to run the test suite? Used to
/// redirect a denied `run_command` to `run_verification`.
pub(super) fn looks_like_test_command(cmd: Option<&str>) -> bool {
    let c = cmd.unwrap_or_default().to_ascii_lowercase();
    c.contains("pytest")
        || c.contains("cargo test")
        || c.contains("npm test")
        || c.contains("go test")
        || (c.contains("test") && c.contains("python"))
}

/// Does an observation read like a failure the model must react to?
///
/// **Only the status line is examined, never the payload.** Every tool answers with a
/// status line and then, often, the thing it fetched — and real source is full of the
/// words this looks for. Scanning the whole observation marked
/// `read_file pylint/config/exceptions.py (23 lines): ...` as a failure purely because
/// the file it returned defines exception classes. Measured on qwen3-coder-30b: three
/// of four reads in a run were flagged, the model was told its successful reads had
/// failed, and it re-read the same files instead of editing. Files with no such word
/// (`pylint/__init__.py`) read clean, which is what made the correlation obvious.
///
/// The status line is authored by the harness, so matching against it is matching
/// against something we control rather than against arbitrary user code.
pub(super) fn looks_like_failure(obs: &str) -> bool {
    let l = obs.lines().next().unwrap_or_default().to_ascii_lowercase();
    // A green verification says "all N passed ✓"; a red one says "K failed".
    // "passed" with no "failed" must NOT read as a failure, so check failure
    // markers but exclude the all-passed phrasing.
    if l.contains("passed") && !l.contains("failed") && !l.contains("error") {
        return false;
    }
    l.contains("error")
        || l.contains("rejected")
        || l.contains("not found")
        || l.contains("no match")
        || l.contains("failed")
        || l.contains("exited non-zero")
}

#[cfg(test)]
mod tests {
    /// A named function read is source, not command output: it gets the file cap.
    ///
    /// It took the 200-line command cap while ALSO taking the paged-read path, so a
    /// long function was rationed a page at a time even though the model had asked
    /// for it by name.
    #[test]
    fn read_function_gets_the_same_generous_cap_as_read_file() {
        let cfg = AgentConfig::default();
        assert_eq!(
            observation_cap_for("read_function", &cfg),
            cfg.read_file_line_cap,
            "a function read is a file read"
        );
        assert_eq!(
            observation_cap_for("run_command", &cfg),
            cfg.observation_line_cap,
            "command output keeps the tight cap"
        );
    }

    use super::*;
    use crate::confirm::Confirmation;
    use sc_context::truncate_observation;
    use serde_json::json;
    use std::sync::Mutex;

    use super::super::test_util::temp_dir;

    /// The regression: real source is full of the words this heuristic looks for, so
    /// scanning the payload made a *successful* read of an exceptions module report as
    /// a failure. Only the status line — which the harness itself writes — is examined.
    #[test]
    fn a_successful_read_of_error_handling_code_is_not_a_failure() {
        let obs = concat!(
            "read_file pylint/config/exceptions.py (23 lines):\n",
            "class _UnrecognizedOptionError(Exception):\n",
            "    \"\"\"Raised if an unrecognized option is encountered.\"\"\"\n",
            "class ArgumentPreprocessingError(Exception):\n",
            "    \"\"\"Raised if an error occurs during argument pre-processing.\"\"\"",
        );
        assert!(
            !looks_like_failure(obs),
            "reading a file that mentions errors is not an error"
        );

        // A test file naming the behaviour under test — the thing a model most needs
        // to read when it has been asked to make that test pass.
        let obs = concat!(
            "read_file tests/config/test_config.py (111 lines):\n",
            "with pytest.raises(_UnrecognizedOptionError):",
        );
        assert!(!looks_like_failure(obs));
    }

    #[test]
    fn a_real_failure_status_line_still_reads_as_one() {
        assert!(looks_like_failure(
            "read_file nope.py error: The system cannot find the path specified."
        ));
        assert!(looks_like_failure("search_code \"foo\": no matches"));
        assert!(looks_like_failure(
            "run_verification: 2 failed, 6 passed:\nFAILED tests/x.py::test_a"
        ));
        assert!(looks_like_failure("edit_file x.py rejected: frozen path"));
    }

    /// **A no-op write is not a failure.**
    ///
    /// A writer that changed no bytes now says so (`sc_tools::builtin::write`'s `no_op`)
    /// instead of the old lie, "ok (1 replacement)". That observation must reach the model
    /// as an ordinary result: the tool worked, the request was vacuous. Wording it as an
    /// error would push the loop's error-reaction machinery at a harmless turn — and the
    /// error path is not what makes a model stop repeating a no-op; the stall detector is,
    /// and it counts the repeat either way.
    #[test]
    fn a_no_op_write_is_not_a_failure() {
        for obs in [
            "edit_file a.rs no-op (nothing written): old_str and new_str are identical, so \
             the replacement changed nothing.",
            "write_file a.txt no-op (nothing written): the content is byte-for-byte what \
             the file already holds.",
            "append_file a.css no-op (nothing written): content is empty, so nothing was \
             appended.",
            "edit_lines a.rs no-op (nothing written): new_text is identical to the lines it \
             would replace.",
            "edit_function m.rs:pick no-op (nothing written): new_body is identical to the \
             function already in the file.",
        ] {
            assert!(
                !looks_like_failure(obs),
                "a vacuous edit is not a hard error: {obs}"
            );
        }
    }

    /// A green verification carries "passed" and no failure marker.
    #[test]
    fn a_green_verification_is_not_a_failure() {
        assert!(!looks_like_failure(
            "run_verification: all 8 test(s) passed"
        ));
    }
    use super::super::AgentConfig;

    #[test]
    fn verify_feedback_keeps_the_underlying_exception() {
        // The auto-verify feedback is truncated with read_file_line_cap, not the tight
        // log cap — so a deep TemplateNotFound/AttributeError survives instead of being
        // crowded out by the ✗/assert headers (the live bug: the model saw only `assert`).
        let mut fb = String::from("(harness ran the tests after your edit)\n");
        for i in 0..60 {
            fb.push_str(&format!("✗ test_app.py::test_{i}\n    assert 500 == 200\n"));
        }
        fb.push_str("E   jinja2.exceptions.TemplateNotFound: board.html\n");
        let cfg = AgentConfig::default();
        let kept = truncate_observation(&fb, cfg.read_file_line_cap, true);
        assert!(
            kept.contains("TemplateNotFound"),
            "the underlying exception must survive truncation"
        );
        // And the tight log cap would have been at risk — document the contrast.
        assert_eq!(
            observation_cap_for("run_verification", &cfg),
            cfg.read_file_line_cap
        );
    }

    #[test]
    fn read_file_and_verification_get_a_generous_cap_but_logs_stay_tight() {
        // A read_file is source the model edits, and a verification report carries the
        // underlying exception — both get read_file_line_cap. A runaway shell log
        // (run_command) and a dir listing keep the tight default where error-first
        // truncation does the work.
        let cfg = AgentConfig {
            observation_line_cap: 40,
            read_file_line_cap: 400,
            ..AgentConfig::default()
        };
        assert_eq!(observation_cap_for("read_file", &cfg), 400);
        assert_eq!(observation_cap_for("run_verification", &cfg), 400);
        assert_eq!(observation_cap_for("run_command", &cfg), 40);
        assert_eq!(observation_cap_for("list_dir", &cfg), 40);
    }

    /// A paged read cut cleanly is a page boundary, not a blind cut: the kept region
    /// is contiguous and the note names the `start` that resumes it, so the model can
    /// recover the rest by asking. Raising `ObservationTruncated` for that would fire
    /// on every page turn of a large file.
    #[test]
    fn a_cut_paged_read_is_not_a_blind_cut() {
        use sc_context::truncate_paged_read;

        let body: String = (2800..=5800)
            .map(|i| {
                format!(
                    "{i}: // line {i}
"
                )
            })
            .collect();
        let obs = format!(
            "read_file big.rs (lines 2800-5800 of 8000):
{body}"
        );
        let trimmed = truncate_paged_read(&obs, 800);

        assert!(
            trimmed.contains("pass start=3598"),
            "the model is told how to continue"
        );
        assert_eq!(
            blind_cut(&obs, &trimmed),
            None,
            "a contiguous prefix that names its next page is not a blind cut"
        );
    }

    /// ...and the genuinely blind cut it exists to catch still fires.
    #[test]
    fn a_head_tail_slice_of_an_error_free_log_still_raises() {
        let obs: String = (1..=2000)
            .map(|i| {
                format!(
                    "quiet line {i}
"
                )
            })
            .collect::<String>();
        let trimmed = truncate_observation(&obs, 200, true);

        assert!(
            trimmed.contains("line(s) truncated]"),
            "no error line to anchor on, so it is a head/tail slice"
        );
        let (total, shown) = blind_cut(&obs, &trimmed).expect("the blind cut must still be caught");
        assert_eq!(total, 2000);
        assert_eq!(shown, 200);
    }

    // --- Confirm-gated run_command (spec 04 / spec 06) -----------------------

    /// Records every command it's asked about and answers with a canned decision.
    struct FakeConfirmer {
        answer: Confirmation,
        seen: Mutex<Vec<String>>,
    }
    impl FakeConfirmer {
        fn new(answer: Confirmation) -> Self {
            Self {
                answer,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }
    impl Confirmer for FakeConfirmer {
        fn confirm_command(&self, command: &str, _default_reason: &str) -> Confirmation {
            self.seen.lock().unwrap().push(command.to_string());
            self.answer.clone()
        }
    }

    fn run_command_call(cmd: &str) -> sc_tools::ValidatedCall {
        let mut args = std::collections::BTreeMap::new();
        args.insert("command".to_string(), json!(cmd));
        sc_tools::ValidatedCall {
            name: "run_command".to_string(),
            args,
        }
    }

    /// `dispatch` with the default (shell-denying) policy, a temp workspace, and a
    /// caller-supplied confirmer + session allowlist. Returns the observation text.
    fn dispatch_run_command(
        cmd: &str,
        confirmer: Option<&dyn Confirmer>,
        session_allow: &mut Vec<String>,
        dry_run: bool,
    ) -> String {
        let ws = temp_dir("confirm");
        let registry = sc_tools::default_registry();
        let policy = PermissionPolicy::default(); // shell denied
        let outcome = dispatch(
            &run_command_call(cmd),
            &registry,
            &policy,
            confirmer,
            session_allow,
            &sc_verify::Sandbox::Host,
            &None,
            dry_run,
            &ws,
            None,
        );
        let _ = std::fs::remove_dir_all(&ws);
        match outcome {
            ToolOutcome::Observation(s) => s,
            _ => panic!("expected an Observation from run_command dispatch"),
        }
    }

    fn read_call(path: &str, start: Option<i64>, limit: Option<i64>) -> sc_tools::ValidatedCall {
        let mut args = std::collections::BTreeMap::new();
        args.insert("path".to_string(), json!(path));
        if let Some(s) = start {
            args.insert("start".to_string(), json!(s));
        }
        if let Some(l) = limit {
            args.insert("limit".to_string(), json!(l));
        }
        sc_tools::ValidatedCall {
            name: "read_file".to_string(),
            args,
        }
    }

    /// **`finish`'s summary is the answer, and must survive into the event.**
    ///
    /// It was not in `key_arg`'s key list, so a read-only run's whole answer arrived at the
    /// UI as an empty string while the run reported success.
    #[test]
    fn key_arg_carries_a_finish_summary() {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "summary".to_string(),
            json!("starfield.rs:173 swaps the colors"),
        );
        let call = sc_tools::ValidatedCall {
            name: "finish".to_string(),
            args,
        };
        assert_eq!(key_arg(&call), "starfield.rs:173 swaps the colors");
    }

    /// Shell commands must hash by what they RUN.
    ///
    /// `key_arg` looked only at `path`/`query`/`name`, and `run_command`'s parameter
    /// is `command` -- so every shell call hashed to the same empty key. Two
    /// different commands read as a repeat (a false stall), and a model genuinely
    /// looping on one command was indistinguishable from one making progress.
    #[test]
    fn key_arg_distinguishes_shell_commands() {
        use crate::recovery::action_hash;

        let ls = run_command_call("ls -la");
        let build = run_command_call("cargo test");
        assert_ne!(
            key_arg(&ls),
            key_arg(&build),
            "different commands must be different actions"
        );
        assert_ne!(
            action_hash("run_command", &key_arg(&ls)),
            action_hash("run_command", &key_arg(&build)),
            "different commands must not hash as a repeat"
        );
        assert!(!key_arg(&ls).is_empty(), "a shell call must have a key");
    }

    /// Two commands that EXECUTE identically must hash identically.
    ///
    /// The dispatch strips a leading `cd`, so `cd /a && ls` and `cd /b && ls` both
    /// run as plain `ls`. If the key kept the raw text, a model looping on one
    /// command while varying the directory would slip past the stall detector.
    #[test]
    fn key_arg_normalizes_a_leading_cd_so_a_real_loop_is_caught() {
        use crate::recovery::action_hash;

        let a = run_command_call("cd /somewhere && ls");
        let b = run_command_call("cd /elsewhere && ls");
        let bare = run_command_call("ls");

        assert_eq!(key_arg(&a), key_arg(&b), "same command, different cd");
        assert_eq!(key_arg(&a), key_arg(&bare), "and the same as no cd at all");
        assert_eq!(
            action_hash("run_command", &key_arg(&a)),
            action_hash("run_command", &key_arg(&bare)),
            "a repeat must be visible to the stall detector"
        );
    }

    #[test]
    fn key_arg_distinguishes_read_windows() {
        use crate::recovery::action_hash;
        // The bug: paging through a file was refused as a duplicate because the key
        // ignored start/limit, so any file past the first window was unreachable.
        let page1 = read_call("db.rs", Some(1), Some(50));
        let page2 = read_call("db.rs", Some(51), Some(50));
        assert_ne!(
            key_arg(&page1),
            key_arg(&page2),
            "different windows of the same file must be distinct actions"
        );
        assert_ne!(
            action_hash("read_file", &key_arg(&page1)),
            action_hash("read_file", &key_arg(&page2)),
            "paging forward must not hash as a repeat"
        );

        // A bare re-read (no window) is still a duplicate — the case we DO want to nudge.
        let bare_a = read_call("db.rs", None, None);
        let bare_b = read_call("db.rs", None, None);
        assert_eq!(key_arg(&bare_a), key_arg(&bare_b));
        assert_eq!(
            key_arg(&bare_a),
            "db.rs",
            "unwindowed read keeps the plain path key"
        );

        // The identical window twice still dedups (genuine re-read of the same page).
        assert_eq!(
            key_arg(&page1),
            key_arg(&read_call("db.rs", Some(1), Some(50)))
        );
    }

    fn edit_call(path: &str, old: &str, new: &str) -> sc_tools::ValidatedCall {
        let mut args = std::collections::BTreeMap::new();
        args.insert("path".to_string(), json!(path));
        args.insert("old_str".to_string(), json!(old));
        args.insert("new_str".to_string(), json!(new));
        sc_tools::ValidatedCall {
            name: "edit_file".to_string(),
            args,
        }
    }

    /// **An edit's identity is its path AND its anchor.**
    ///
    /// The fifth instance of `key_arg`'s recurring bug, and the worst: keying an edit on
    /// `path` alone made every edit of one file the same action, so three successful edits
    /// to `lib.rs` tripped `repeat_limit` (3) and the model was told to STOP while it was
    /// turning tests green.
    #[test]
    fn action_key_distinguishes_two_edits_to_the_same_file() {
        use crate::recovery::action_hash;

        let first = edit_call("lib.rs", "let n = 1;", "let n = 2;");
        let second = edit_call(
            "lib.rs",
            "fn evict(&mut self) {",
            "fn evict(&mut self, n: u8) {",
        );
        assert_ne!(
            action_key(&first),
            action_key(&second),
            "two different edits to one file must be different actions"
        );
        assert_ne!(
            action_hash("edit_file", &action_key(&first)),
            action_hash("edit_file", &action_key(&second)),
            "and must not hash as a repeat"
        );

        // The same edit twice IS a repeat -- the case the detector must still catch.
        assert_eq!(
            action_key(&first),
            action_key(&edit_call("lib.rs", "let n = 1;", "let n = 2;"))
        );

        // Shape: the path in the clear, plus a fixed-width suffix however big the anchor.
        let k = action_key(&first);
        assert!(k.starts_with("lib.rs#"), "{k}");
        assert_eq!(k.len(), "lib.rs#".len() + 8, "{k}");
        let huge = edit_call("lib.rs", &"x".repeat(20_000), "y");
        assert_eq!(
            action_key(&huge).len(),
            k.len(),
            "a kilobyte anchor must not bloat the key"
        );

        // A different FILE is still a different action, same anchor or not.
        assert_ne!(
            action_key(&first),
            action_key(&edit_call("other.rs", "let n = 1;", "let n = 2;"))
        );
    }

    /// **`key_arg` itself stays a PATH.** The loop reads the file off disk with it and
    /// names it to the model in the rewrite directive, so the anchor must never leak in:
    /// appending it there made the harness order a `write_file` to `big.rs#249d985d`.
    #[test]
    fn key_arg_stays_a_bare_path_for_an_edit() {
        let e = edit_call("big.rs", "let n = 1;", "let n = 2;");
        assert_eq!(key_arg(&e), "big.rs");
        assert!(!key_arg(&e).contains('#'));
    }

    /// The other two editors anchor too: `edit_lines` on its full range (start alone is not
    /// enough -- 1..5 and 1..9 are different edits) and `edit_function` on the name.
    #[test]
    fn action_key_distinguishes_line_and_function_edits() {
        let lines = |start: i64, end: i64| {
            let mut args = std::collections::BTreeMap::new();
            args.insert("path".to_string(), json!("lib.rs"));
            args.insert("start".to_string(), json!(start));
            args.insert("end".to_string(), json!(end));
            args.insert("new_text".to_string(), json!("// x"));
            sc_tools::ValidatedCall {
                name: "edit_lines".to_string(),
                args,
            }
        };
        assert_ne!(
            action_key(&lines(1, 5)),
            action_key(&lines(1, 9)),
            "same start, different end"
        );
        assert_ne!(action_key(&lines(1, 5)), action_key(&lines(20, 24)));
        assert_eq!(action_key(&lines(1, 5)), action_key(&lines(1, 5)));

        let func = |name: &str| {
            let mut args = std::collections::BTreeMap::new();
            args.insert("path".to_string(), json!("lib.rs"));
            args.insert("name".to_string(), json!(name));
            args.insert("new_body".to_string(), json!("fn f() {}"));
            sc_tools::ValidatedCall {
                name: "edit_function".to_string(),
                args,
            }
        };
        assert_ne!(action_key(&func("evict")), action_key(&func("insert")));
        assert_eq!(action_key(&func("evict")), action_key(&func("evict")));
    }

    /// A whole-file write has no anchor, so it keeps the plain path: rewriting one file
    /// three times running must still read as a repeat. (That rewrite is exactly the
    /// escalation the false loop pushed the model into, so the detector must still catch
    /// it.)
    #[test]
    fn a_whole_file_write_keeps_the_plain_path_key() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("path".to_string(), json!("lib.rs"));
        args.insert("content".to_string(), json!("fn main() {}"));
        let a = sc_tools::ValidatedCall {
            name: "write_file".to_string(),
            args: args.clone(),
        };
        assert_eq!(action_key(&a), "lib.rs");

        args.insert("content".to_string(), json!("fn main() { other(); }"));
        let b = sc_tools::ValidatedCall {
            name: "write_file".to_string(),
            args,
        };
        assert_eq!(action_key(&b), action_key(&a), "a rewrite is a rewrite");
    }

    /// Every non-edit tool's action identity is untouched by the anchor split.
    #[test]
    fn action_key_matches_key_arg_for_every_non_edit_tool() {
        let reads = read_call("db.rs", Some(51), Some(50));
        assert_eq!(action_key(&reads), key_arg(&reads));
        let cmd = run_command_call("ls -la");
        assert_eq!(action_key(&cmd), key_arg(&cmd));
    }

    #[test]
    fn unapproved_shell_denied_when_no_confirmer() {
        // No confirmer ⇒ today's behavior: the static Deny stands.
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", None, &mut allow, false);
        assert!(obs.contains("denied"), "{obs}");
        assert!(!obs.contains("exited"), "command must not run: {obs}");
        assert!(allow.is_empty());
    }

    #[test]
    fn confirmer_allow_once_runs_otherwise_denied_command() {
        let fake = FakeConfirmer::new(Confirmation::AllowOnce);
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", Some(&fake), &mut allow, false);
        assert!(obs.contains("exited"), "command should have run: {obs}");
        assert_eq!(fake.calls(), 1);
        assert!(allow.is_empty(), "AllowOnce must not remember anything");
    }

    #[test]
    fn confirmer_deny_blocks_command() {
        let fake = FakeConfirmer::new(Confirmation::Deny("nope".to_string()));
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", Some(&fake), &mut allow, false);
        assert!(obs.contains("denied: nope"), "{obs}");
        assert!(!obs.contains("exited"), "command must not run: {obs}");
    }

    #[test]
    fn remember_mutates_effective_allowlist_for_rest_of_run() {
        let fake = FakeConfirmer::new(Confirmation::AllowRemember {
            prefix: "echo ".to_string(),
        });
        let mut allow = Vec::new();

        // First matching command: prompts once, runs, and remembers the prefix.
        let first = dispatch_run_command("echo one", Some(&fake), &mut allow, false);
        assert!(first.contains("exited"), "{first}");
        assert_eq!(allow, vec!["echo ".to_string()]);

        // Second matching command: runs WITHOUT consulting the confirmer again.
        let second = dispatch_run_command("echo two", Some(&fake), &mut allow, false);
        assert!(second.contains("exited"), "{second}");
        assert_eq!(
            fake.calls(),
            1,
            "remembered prefix must short-circuit the gate"
        );
    }

    #[test]
    fn test_command_redirect_still_wins_over_confirmer() {
        // The pytest→run_verification redirect precedes prompting, so the confirmer
        // is never consulted for a test command.
        let fake = FakeConfirmer::new(Confirmation::AllowOnce);
        let mut allow = Vec::new();
        let obs = dispatch_run_command("pytest", Some(&fake), &mut allow, false);
        assert!(obs.contains("run_verification"), "{obs}");
        assert_eq!(
            fake.calls(),
            0,
            "confirmer must not be consulted for a test cmd"
        );
    }

    #[test]
    fn dry_run_honored_even_when_confirmer_allows() {
        // A human-approved command still respects --dry-run: no process is spawned.
        let fake = FakeConfirmer::new(Confirmation::AllowOnce);
        let mut allow = Vec::new();
        let obs = dispatch_run_command("echo hi", Some(&fake), &mut allow, true);
        assert!(obs.contains("[dry-run]"), "{obs}");
        assert!(!obs.contains("exited"), "dry-run must not execute: {obs}");
    }
}
