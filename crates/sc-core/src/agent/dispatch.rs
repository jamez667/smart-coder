//! Tool execution and its surrounding classifiers: the permission/dry-run gate + routing
//! for a single validated call, the `finish` whole-suite gate, the batched-write pre-apply,
//! and the small predicates the loop uses to decide how to treat a call or its observation.

use std::path::Path;

use sc_tools::{execute, Journal, PermissionPolicy, ToolOutcome, ToolRegistry};

use crate::confirm::{Confirmation, Confirmer};
use crate::event::{AgentEvent, EventSink};
use crate::text::first_line;

use super::AgentConfig;

/// Outcome of the whole-suite gate at `finish`.
pub(super) enum FinishGate {
    /// Finish is honored; the bool is the verified state (None → no verify cmd).
    Allow(Option<bool>),
    /// Finish is refused with an observation the model must react to.
    Refuse(String),
}

/// Run the configured verification before honoring `finish` (spec 11). With no
/// command configured, finish is always allowed (verified = None).
pub(super) fn gate_finish(
    sandbox: &sc_verify::Sandbox,
    verify_command: &Option<String>,
    workspace: &Path,
) -> FinishGate {
    match verify_command {
        None => FinishGate::Allow(None),
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

    match call.name.as_str() {
        "find_symbol" => {
            let name = call.str("name").unwrap_or_default();
            ToolOutcome::Observation(sc_index::find_symbol(workspace, name))
        }
        "run_command" => {
            let cmd = call.str("command").unwrap_or_default();
            let r = sc_verify::run_command(workspace, cmd);
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

/// Tools whose result is fully determined by the current workspace + args, so
/// issuing the *same* call twice in a row (with nothing changed between) yields
/// the same observation — used by the repeat-dedup nudge. `run_verification` is
/// included: re-running the suite without an intervening edit can only reprint
/// the same failures, and a tiny model loves to re-verify instead of fixing.
pub(super) fn is_idempotent_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file" | "list_dir" | "search_code" | "find_symbol" | "run_verification"
    )
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
        "read_file" | "run_verification" => cfg.read_file_line_cap,
        _ => cfg.observation_line_cap,
    }
}

/// The key argument of a call, for the repeat-dedup history record (path or
/// query/name). For a windowed `read_file` the window (`start`/`limit`) is folded
/// into the key so paging THROUGH a file — `read_file(a.rs, start=1)` then
/// `read_file(a.rs, start=51)` — reads as two DISTINCT actions, not a refused
/// "duplicate". Without this, any file past the first window is unreachable: the
/// second page hashes identical to the first and gets nudged away, so the model
/// can never see lines 51+ of a file it must edit. A bare re-read (same path, no
/// window, or the identical window) still dedups, which is the case we want to nudge.
pub(super) fn key_arg(call: &sc_tools::ValidatedCall) -> String {
    for k in ["path", "query", "name"] {
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
pub(super) fn looks_like_failure(obs: &str) -> bool {
    let l = obs.to_ascii_lowercase();
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
    use super::*;
    use crate::confirm::Confirmation;
    use sc_context::truncate_observation;
    use serde_json::json;
    use std::sync::Mutex;

    use super::super::test_util::temp_dir;
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
