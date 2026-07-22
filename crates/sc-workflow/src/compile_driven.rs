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

/// A single planned unit of work handed to the builder: a scoped goal + the files it touches +
/// the ids it depends on (so the builder applies them in dependency order). This is the shape the
/// caller flattens its decomposition board into.
#[derive(Debug, Clone)]
pub struct BuildTask {
    pub id: String,
    pub goal: String,
    pub files: Vec<String>,
    pub deps: Vec<String>,
}

/// One diagnostic-driven fix attempt, for the caller's event stream.
#[derive(Debug, Clone)]
pub enum BuildEvent {
    /// Applying the foundational change (the chunk the decomposition named).
    Foundational { goal: String },
    /// Applying a planned subtask (one of the decomposition's units), in dependency order.
    /// `index`/`total` let the UI show progress (`building 2/7`).
    Subtask {
        id: String,
        goal: String,
        index: usize,
        total: usize,
    },
    /// A verify pass ran; `errors` is how many compile errors it found (0 = green).
    Checked { errors: usize },
    /// About to fix a specific located diagnostic.
    Fixing {
        file: String,
        line: u32,
        message: String,
    },
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

/// If the error count doesn't IMPROVE for this many consecutive check→fix rounds, the loop isn't
/// converging (e.g. a delimiter cascade where rustc reports the symptom line, not the cause — the
/// model edits the wrong place forever). Bail instead of grinding the whole iteration budget.
const STALL_LIMIT: usize = 2;

/// What the fix loop should do after a verify pass — the pure termination decision, extracted from
/// the I/O so it's exhaustively testable without a real backend or sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopStep {
    /// No errors — stop; `green` is whether the verify command itself succeeded.
    Green,
    /// Give up: the iteration budget is spent OR the loop stalled (no improvement).
    GiveUp,
    /// Keep going — run a fix pass over the current errors, then re-check.
    Fix,
}

/// The termination state machine for the check→fix loop. Pure: `record(error_count)` folds one
/// verify result in and returns the next [`LoopStep`]. This is where "does the loop always
/// terminate?" is decided — proven by unit tests over rising / flat / oscillating / converging
/// error sequences, with no model or sandbox in the loop.
#[derive(Debug, Clone, Copy)]
struct LoopState {
    iterations: usize,
    prev_error_count: usize,
    stalls: usize,
}

impl LoopState {
    fn new() -> Self {
        Self {
            iterations: 0,
            prev_error_count: usize::MAX,
            stalls: 0,
        }
    }

    /// Fold in one verify result (`error_count` = compile errors this round) and decide the next
    /// step. Advances `iterations` only when the answer is [`LoopStep::Fix`] (a fix pass is about
    /// to run), so `iterations` counts fix rounds, not verify passes.
    fn record(&mut self, error_count: usize) -> LoopStep {
        if error_count == 0 {
            return LoopStep::Green;
        }
        // A stall is a round whose error total did NOT drop vs. the previous round.
        if error_count >= self.prev_error_count {
            self.stalls += 1;
        } else {
            self.stalls = 0;
        }
        self.prev_error_count = error_count;
        if self.iterations >= MAX_ITERATIONS || self.stalls >= STALL_LIMIT {
            return LoopStep::GiveUp;
        }
        self.iterations += 1;
        LoopStep::Fix
    }
}

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
    on_agent: &dyn Fn(&sc_core::AgentEvent),
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
        on_agent,
    );

    verify_fix_loop(
        backend,
        workspace,
        sandbox,
        verify_command,
        on_event,
        on_agent,
    )
}

/// Build the WHOLE decomposition, not just the foundational chunk: apply every `tasks` unit as a
/// scoped edit in dependency order, THEN run the compiler-driven verify→fix loop to integrate them.
///
/// This is the fix for a build stopping after one file: `build_compiler_driven` applies only the
/// single foundational subtask and relies on the compiler surfacing the rest — but when that first
/// change compiles cleanly in isolation (e.g. a standalone new enum file), nothing cascades and the
/// other subtasks are never built. Applying every planned subtask first guarantees each named file
/// is created/edited; the verify→fix loop then resolves the integration errors between them.
pub fn build_all_subtasks(
    backend: &dyn ModelBackend,
    workspace: &Path,
    sandbox: &Sandbox,
    verify_command: &str,
    tasks: &[BuildTask],
    on_event: &dyn Fn(BuildEvent),
    on_agent: &dyn Fn(&sc_core::AgentEvent),
) -> BuildOutcome {
    let ordered = order_by_deps(tasks);
    let total = ordered.len();
    for (i, t) in ordered.iter().enumerate() {
        on_event(BuildEvent::Subtask {
            id: t.id.clone(),
            goal: t.goal.clone(),
            index: i + 1,
            total,
        });
        run_scoped_edit(backend, workspace, sandbox, &t.files, &t.goal, on_agent);
    }
    verify_fix_loop(
        backend,
        workspace,
        sandbox,
        verify_command,
        on_event,
        on_agent,
    )
}

/// Order `tasks` so a task's `deps` come before it — a topological walk that falls back to the
/// input order for a cycle/dangling dep (never strands a task). Mirrors the swarm board's walk so
/// the build applies foundational units (the ones with no deps) first.
fn order_by_deps(tasks: &[BuildTask]) -> Vec<BuildTask> {
    let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let mut emitted: Vec<String> = Vec::new();
    let mut out: Vec<BuildTask> = Vec::new();
    while out.len() < tasks.len() {
        let next = tasks
            .iter()
            .filter(|t| !emitted.contains(&t.id))
            .find(|t| t.deps.iter().all(|d| emitted.contains(d)))
            .or_else(|| {
                // cycle / dep on a missing id: take the first remaining, don't strand it.
                tasks.iter().find(|t| !emitted.contains(&t.id))
            });
        match next {
            Some(t) => {
                emitted.push(t.id.clone());
                out.push(t.clone());
            }
            None => break,
        }
    }
    // Defensive: if anything was missed (shouldn't happen), append the rest in input order.
    for t in tasks {
        if !emitted.contains(&t.id) {
            out.push(t.clone());
        }
    }
    let _ = ids;
    out
}

/// The shared verify → fix-each-diagnostic loop: run the verify command, parse compile errors, fix
/// each with a scoped single-shot pass, repeat until green or the iteration budget is spent.
fn verify_fix_loop(
    backend: &dyn ModelBackend,
    workspace: &Path,
    sandbox: &Sandbox,
    verify_command: &str,
    on_event: &dyn Fn(BuildEvent),
    on_agent: &dyn Fn(&sc_core::AgentEvent),
) -> BuildOutcome {
    let mut state = LoopState::new();
    loop {
        let result = run_command_in(sandbox, workspace, verify_command);
        let errors = compile_errors(&result.output);
        on_event(BuildEvent::Checked {
            errors: errors.len(),
        });
        match state.record(errors.len()) {
            LoopStep::Green => {
                on_event(BuildEvent::Done {
                    green: result.ok,
                    iterations: state.iterations,
                });
                return BuildOutcome {
                    green: result.ok,
                    iterations: state.iterations,
                    remaining: Vec::new(),
                };
            }
            LoopStep::GiveUp => {
                on_event(BuildEvent::Done {
                    green: false,
                    iterations: state.iterations,
                });
                return BuildOutcome {
                    green: false,
                    iterations: state.iterations,
                    remaining: errors,
                };
            }
            LoopStep::Fix => {}
        }

        // 3) Each diagnostic → a scoped single-shot fix. Delimiter/brace errors get extra guidance
        // because rustc reports the SYMPTOM line, not the cause — a naive "fix exactly this line"
        // makes the model edit the wrong place and loop (observed live 2026-07-21: widgets.rs looped
        // on line 320 while the real unclosed `{` was at 539, from a duplicated block).
        for e in errors.iter().take(MAX_FIXES_PER_ITER) {
            on_event(BuildEvent::Fixing {
                file: e.file.clone(),
                line: e.line,
                message: e.message.clone(),
            });
            let is_delimiter = {
                let m = e.message.to_ascii_lowercase();
                m.contains("delimiter")
                    || m.contains("unclosed")
                    || m.contains("mismatched")
                    || m.contains("expected `}`")
                    || m.contains("expected `)`")
            };
            let hint = if is_delimiter {
                "This is a DELIMITER/BRACE error — the reported line is where the compiler NOTICED \
                 the imbalance, NOT necessarily the cause. Read the whole function/region around it \
                 and find the ACTUAL unbalanced `{`/`}`/`(`/`)` — it is often EARLIER, and often a \
                 DUPLICATED block (a function or struct pasted twice) or a missing/extra closing \
                 brace. Remove the duplicate or balance the braces. "
            } else {
                "If it is a non-exhaustive match, add the missing arm(s); if a signature/type \
                 mismatch, correct it minimally. "
            };
            let goal = format!(
                "There is a compile error in `{}` at line {}:\n  {}\n\n{}Make one small, idiomatic \
                 edit (use edit_function for a match arm or body), then finish. The relevant code is \
                 in `{}` (pinned below).",
                e.file, e.line, e.message, hint, e.file
            );
            run_scoped_edit(
                backend,
                workspace,
                sandbox,
                &[e.file.clone()],
                &goal,
                on_agent,
            );
        }
    }
}

/// Run one scoped, single-purpose agent pass: focused on `files` (their live contents are pinned
/// each turn, so the model edits rather than hunts), a tight step budget, told to make one edit.
/// Edits the real workspace. Its `AgentEvent`s are forwarded to `on_agent` so the caller can
/// surface them (count touched files, stream edits into the chat / code view) — previously they
/// were swallowed, which made a genuinely-successful build report "0 files touched". The verify
/// pass remains the source of truth for whether the edit worked.
fn run_scoped_edit(
    backend: &dyn ModelBackend,
    workspace: &Path,
    sandbox: &Sandbox,
    files: &[String],
    goal: &str,
    on_agent: &dyn Fn(&sc_core::AgentEvent),
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
    let sink = FnSink(on_agent);
    let sink: &dyn EventSink = &sink;
    let _ = run_agent_observed(
        backend,
        None,
        &registry,
        strategy.as_ref(),
        goal,
        workspace,
        &cfg,
        sink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the pure loop state machine over a scripted sequence of per-round error counts and
    /// return (steps_taken, final_step). This is the deterministic stand-in for the real loop: the
    /// model/sandbox only ever influence the loop through the error count, so a scripted sequence
    /// exercises every termination path with no backend.
    fn run_sequence(errors: &[usize]) -> (usize, LoopStep) {
        let mut state = LoopState::new();
        let mut steps = 0;
        for &e in errors {
            steps += 1;
            match state.record(e) {
                LoopStep::Fix => continue,
                stop => return (steps, stop),
            }
        }
        // Ran out of scripted rounds without terminating — the loop would keep going. The test
        // asserts this never happens within the bound.
        (steps, LoopStep::Fix)
    }

    #[test]
    fn loop_converges_to_green() {
        // A healthy build: errors drop each round, then hit zero → Green.
        let (steps, end) = run_sequence(&[5, 3, 1, 0]);
        assert_eq!(end, LoopStep::Green);
        assert_eq!(steps, 4);
    }

    #[test]
    fn loop_bails_on_a_flat_error_count() {
        // The delimiter-cascade shape: the same error every round. After STALL_LIMIT (2)
        // non-improving rounds it must GiveUp — NOT grind the whole iteration budget.
        let (_steps, end) = run_sequence(&[3, 3, 3, 3, 3, 3, 3, 3]);
        assert_eq!(end, LoopStep::GiveUp, "flat count must stall out");
    }

    #[test]
    fn loop_bails_on_oscillation() {
        // Error count bounces 5→3→5→3… — never a sustained improvement. The stall counter resets
        // on the 5→3 drop but re-fires on the 3→5 rise, so it still terminates (not an infinite
        // loop). Must GiveUp well within the iteration cap.
        let (steps, end) = run_sequence(&[5, 3, 5, 3, 5, 3, 5, 3, 5, 3]);
        assert_eq!(end, LoopStep::GiveUp);
        assert!(
            steps <= MAX_ITERATIONS + 2,
            "oscillation bounded, took {steps}"
        );
    }

    #[test]
    fn loop_is_bounded_even_when_errors_only_ever_decrease_slowly() {
        // Worst honest case: errors decrease by 1 every round but from a large start — it makes
        // progress, so stalls never fire; termination is the MAX_ITERATIONS backstop. Prove the
        // loop cannot exceed the cap. (A never-improving-fast build still can't run forever.)
        let seq: Vec<usize> = (0..100).rev().collect(); // 99,98,...,1,0
        let (steps, end) = run_sequence(&seq);
        // Either it reached 0 (Green) or hit the iteration cap (GiveUp) — never ran past the bound.
        assert!(
            matches!(end, LoopStep::Green | LoopStep::GiveUp),
            "must terminate"
        );
        assert!(
            steps <= MAX_ITERATIONS + 1,
            "must terminate within the iteration cap, took {steps}"
        );
    }

    #[test]
    fn loop_never_runs_more_than_the_iteration_cap_of_fix_rounds() {
        // Exhaustive-ish: for a broad range of adversarial sequences, the number of Fix rounds
        // (state.iterations) is always <= MAX_ITERATIONS. This is the core "cannot loop forever"
        // property, independent of the error pattern.
        let sequences: &[&[usize]] = &[
            &[7; 20],                      // flat high
            &[1, 1, 1, 1, 1, 1, 1, 1, 1],  // flat low
            &[9, 8, 9, 8, 9, 8, 9, 8, 9],  // oscillate
            &[2, 3, 4, 5, 6, 7, 8, 9, 10], // rising (getting worse)
            &[10, 10, 10, 10, 10, 10, 10], // stuck
        ];
        for seq in sequences {
            let mut state = LoopState::new();
            let mut fix_rounds = 0;
            for &e in seq.iter() {
                match state.record(e) {
                    LoopStep::Fix => fix_rounds += 1,
                    _ => break,
                }
            }
            assert!(
                fix_rounds <= MAX_ITERATIONS,
                "seq {seq:?} ran {fix_rounds} fix rounds (> cap {MAX_ITERATIONS})"
            );
        }
    }

    #[test]
    fn loop_green_reports_the_command_result() {
        // Zero errors on the first check → Green immediately, no fix rounds.
        let mut state = LoopState::new();
        assert_eq!(state.record(0), LoopStep::Green);
        assert_eq!(state.iterations, 0);
    }

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

    fn task(id: &str, deps: &[&str]) -> BuildTask {
        BuildTask {
            id: id.to_string(),
            goal: format!("do {id}"),
            files: vec![format!("{id}.rs")],
            deps: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn order_by_deps_puts_dependencies_first() {
        // t2,t3 depend on t1; t5 depends on t3. A valid order must place each after its deps.
        let tasks = vec![
            task("t3", &["t1"]),
            task("t5", &["t3"]),
            task("t1", &[]),
            task("t2", &["t1"]),
        ];
        let ordered = order_by_deps(&tasks);
        let ids: Vec<&str> = ordered.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ordered.len(), 4, "no task dropped");
        let pos = |id: &str| ids.iter().position(|x| *x == id).unwrap();
        assert!(pos("t1") < pos("t2"), "t1 before t2");
        assert!(pos("t1") < pos("t3"), "t1 before t3");
        assert!(pos("t3") < pos("t5"), "t3 before t5");
    }

    #[test]
    fn order_by_deps_never_strands_a_cycle_or_dangling_dep() {
        // A cycle (t1↔t2) and a dangling dep (t3→missing) must not drop tasks — every id appears.
        let tasks = vec![
            task("t1", &["t2"]),
            task("t2", &["t1"]),
            task("t3", &["nope"]),
        ];
        let ordered = order_by_deps(&tasks);
        assert_eq!(
            ordered.len(),
            3,
            "all tasks emitted despite cycle/dangling dep"
        );
        let ids: std::collections::HashSet<&str> = ordered.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains("t1") && ids.contains("t2") && ids.contains("t3"));
    }
}
