//! Compiler-driven execution — the "discover-then-decompose" builder.
//!
//! A small model can't enumerate the sites a change touches from prose (it decomposes an
//! "add an enum variant and fix every match on it" task down to just the variant, because it
//! doesn't know WHERE the matches are). The compiler does. So instead of pre-decomposing the
//! sites, we let the compiler discover them:
//!
//! 1. Apply the **foundational chunk** (the one change the decomposition CAN name — the new
//!    variant / type / signature), scoped to its file.
//! 2. Run the verify command (`cargo check`). Parse its structured diagnostics
//!    ([`sc_verify::compile_errors`]) into a located work-list: `(file, line, message)`.
//! 3. For each diagnostic, run a **scoped single-shot agent** focused on that one file, told to
//!    fix exactly that error at that line. Tiny context, one edit — the model can't get lost.
//! 4. Re-check and repeat: cascades surface new diagnostics, which become new work-items, until
//!    the check is green or a bounded iteration budget is spent.
//!
//! The compiler is the decomposer; the model only ever makes one small, located edit at a time.

use std::path::Path;

use sc_core::{
    default_registry, run_agent_observed, select_strategy, AgentConfig, EventSink, FnSink,
};
use sc_model::ModelBackend;
use sc_verify::{compile_errors, run_command_in, CompileError, Sandbox};

/// One diagnostic-driven fix attempt, for the caller's event stream.
#[derive(Debug, Clone)]
pub enum BuildEvent {
    /// Applying the foundational change (the chunk the decomposition named).
    Foundational { goal: String },
    /// A verify pass ran; `errors` is how many compile errors it found (0 = green).
    Checked { errors: usize },
    /// About to fix a specific located diagnostic.
    Fixing { file: String, line: u32, message: String },
    /// The build finished. `green` = the final check passed.
    Done { green: bool, iterations: usize },
}

/// The outcome of a compiler-driven build.
#[derive(Debug, Clone)]
pub struct BuildOutcome {
    /// Whether the final verify command was green.
    pub green: bool,
    /// How many check→fix iterations ran.
    pub iterations: usize,
    /// The compile errors still outstanding if it gave up (empty when green).
    pub remaining: Vec<CompileError>,
}

/// Max check→fix iterations before giving up — a backstop against a fix that reintroduces an
/// error each round. Each iteration fixes every error the compiler currently reports, so a real
/// multi-site change converges in 1–2 iterations; the ceiling only catches oscillation.
const MAX_ITERATIONS: usize = 8;

/// Cap on errors fixed per iteration, so a cascade of hundreds can't spawn hundreds of agent
/// calls in one pass. The remainder is caught on the next iteration's re-check.
const MAX_FIXES_PER_ITER: usize = 12;

/// Run a compiler-driven build: apply `foundational_goal` (touching `foundational_files`), then
/// loop verify→fix-each-diagnostic until `verify_command` is green (or the iteration budget is
/// spent). `on_event` receives progress. Edits the real `workspace` in place.
pub fn build_compiler_driven(
    backend: &dyn ModelBackend,
    workspace: &Path,
    sandbox: &Sandbox,
    verify_command: &str,
    foundational_goal: &str,
    foundational_files: &[String],
    on_event: &dyn Fn(BuildEvent),
) -> BuildOutcome {
    // 1) Foundational chunk — the change the decomposition could name.
    on_event(BuildEvent::Foundational {
        goal: foundational_goal.to_string(),
    });
    run_scoped_edit(
        backend,
        workspace,
        sandbox,
        foundational_files,
        foundational_goal,
    );

    // 2) verify → fix loop, compiler-driven.
    let mut iterations = 0;
    loop {
        let result = run_command_in(sandbox, workspace, verify_command);
        let errors = compile_errors(&result.output);
        on_event(BuildEvent::Checked {
            errors: errors.len(),
        });
        if errors.is_empty() {
            on_event(BuildEvent::Done {
                green: result.ok,
                iterations,
            });
            return BuildOutcome {
                green: result.ok,
                iterations,
                remaining: Vec::new(),
            };
        }
        if iterations >= MAX_ITERATIONS {
            on_event(BuildEvent::Done {
                green: false,
                iterations,
            });
            return BuildOutcome {
                green: false,
                iterations,
                remaining: errors,
            };
        }
        iterations += 1;

        // 3) Each diagnostic → a scoped single-shot fix.
        for e in errors.iter().take(MAX_FIXES_PER_ITER) {
            on_event(BuildEvent::Fixing {
                file: e.file.clone(),
                line: e.line,
                message: e.message.clone(),
            });
            let goal = format!(
                "There is a compile error in `{}` at line {}:\n  {}\n\nFix EXACTLY this error and \
                 nothing else. The relevant code is in `{}` (pinned below). If it is a \
                 non-exhaustive match, add the missing arm(s); if a signature/type mismatch, \
                 correct it minimally. Make one small, idiomatic edit (use edit_function for a \
                 match arm or body), then finish.",
                e.file, e.line, e.message, e.file
            );
            run_scoped_edit(backend, workspace, sandbox, &[e.file.clone()], &goal);
        }
    }
}

/// Run one scoped, single-purpose agent pass: focused on `files` (their live contents are pinned
/// each turn, so the model edits rather than hunts), a tight step budget, told to make one edit.
/// Edits the real workspace. Errors/finishes are swallowed — the verify pass is the source of
/// truth for whether the edit worked, so a failed pass just leaves the error for the next round.
fn run_scoped_edit(
    backend: &dyn ModelBackend,
    workspace: &Path,
    sandbox: &Sandbox,
    files: &[String],
    goal: &str,
) {
    let registry = default_registry();
    let strategy = select_strategy(&backend.capabilities());
    let mut cfg = AgentConfig::default();
    cfg.focus_files = files.to_vec();
    cfg.sandbox = sandbox.clone();
    // No verify inside a per-site pass — the outer loop owns verification. A tight budget: the
    // pass has ONE located edit to make, so it shouldn't wander.
    cfg.verify_command = None;
    cfg.max_steps = 6;
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let sink: &dyn EventSink = &sink;
    let _ = run_agent_observed(
        backend, None, &registry, strategy.as_ref(), goal, workspace, &cfg, sink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_outcome_reports_green() {
        // Pure shape check — the executor's decision logic is exercised live; here we just pin
        // the outcome type so the public surface stays stable.
        let o = BuildOutcome {
            green: true,
            iterations: 2,
            remaining: Vec::new(),
        };
        assert!(o.green && o.remaining.is_empty());
    }
}
