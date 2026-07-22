//! Live proof that the compiler-driven fix loop can recover a REAL compile error — specifically
//! the delimiter-cascade class that made a build loop forever (observed live 2026-07-21:
//! `widgets.rs` looped on the SYMPTOM line 320 while the real unclosed `{` — from a duplicated
//! function block — was at line 539).
//!
//! Two things are asserted:
//!   1. **It terminates.** The loop must NOT run forever on an unfixable cascade — stall detection
//!      (error count not decreasing) or the iteration cap bounds it. This is the regression guard:
//!      the bug wasn't "couldn't fix it", it was "ground the token budget looping".
//!   2. **It makes progress or cleanly gives up.** Either the file compiles (`green`), or it stops
//!      with the errors reported — never an infinite loop, never a panic.
//!
//! Uses the REAL local model (via `SC_BASE_URL`/`SC_MODEL`, default llama.cpp :11435) and the HOST
//! sandbox (so `cargo check` actually runs). Ignored by default — it needs a live backend + cargo:
//!   cargo test -p sc-workflow --test live_fix -- --ignored --nocapture

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use sc_model::OpenAiBackend;
use sc_verify::Sandbox;
use sc_workflow::BuildEvent;

/// The backend under test: the local OpenAI-compatible server, overridable by env so this runs
/// against whatever model the box serves. Mirrors how the GUI resolves the coder backend.
fn local_backend() -> OpenAiBackend {
    let base = std::env::var("SC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("SC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    OpenAiBackend::new(base, model).with_detected_context()
}

/// Write a minimal but REAL cargo crate whose `widgets.rs` has the exact failure shape: a function
/// (`draw_modal_shell`) that is DUPLICATED, and the second copy is missing its closing brace — so
/// `cargo check` reports an "unexpected closing delimiter" at a line AFTER the true cause. A naive
/// "fix exactly this line" edit can't resolve it; the real fix is to delete the duplicate.
fn scaffold_broken_crate(dir: &Path) -> String {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"widgets_repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub mod widgets;\n").unwrap();
    // The bug: `draw_modal_shell` appears twice; the SECOND copy has no closing `}` (the duplicated
    // block that broke the real file). rustc flags the delimiter imbalance further down.
    let broken = "\
/// A widget row. This one is fine.
pub fn draw_row(x: i32) -> i32 {
    let y = x + 1;
    y
}

/// Modal shell — the ORIGINAL, correct definition.
pub fn draw_modal_shell(title: &str) -> usize {
    let n = title.len();
    n
}

/// Modal shell — a DUPLICATE the builder pasted in, MISSING its closing brace.
pub fn draw_modal_shell_dup(title: &str) -> usize {
    let n = title.len();
    n
// <-- missing `}` here: the duplicate is unclosed, cascading the error below

/// A trailing function whose `}` the compiler will flag as unexpected.
pub fn draw_footer(w: i32) -> i32 {
    w * 2
}
";
    std::fs::write(dir.join("src/widgets.rs"), broken).unwrap();
    "src/widgets.rs".to_string()
}

#[test]
#[ignore = "live: needs a local model backend + cargo on PATH"]
fn compiler_driven_fix_recovers_or_bails_on_a_delimiter_cascade() {
    let backend = local_backend();
    let dir = std::env::temp_dir().join(format!("dc-live-fix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = scaffold_broken_crate(&dir);
    eprintln!("workspace: {}", dir.display());

    // Confirm the scaffold really is broken before we start (guards the test itself).
    let pre = std::process::Command::new("cargo")
        .arg("check")
        .current_dir(&dir)
        .output()
        .expect("run cargo check");
    assert!(
        !pre.status.success(),
        "scaffold should NOT compile — the repro is invalid"
    );

    // Count fix passes so we can prove the loop TERMINATED (didn't grind forever). Each `Fixing`
    // event is one scoped fix attempt; the whole loop is bounded by iteration + stall limits.
    let fixes = AtomicUsize::new(0);
    let checks = AtomicUsize::new(0);
    let on_event = |e: BuildEvent| match e {
        BuildEvent::Fixing { file, line, message } => {
            fixes.fetch_add(1, Ordering::Relaxed);
            eprintln!("[fix] {file}:{line} — {message}");
        }
        BuildEvent::Checked { errors } => {
            checks.fetch_add(1, Ordering::Relaxed);
            eprintln!("[check] {errors} error(s)");
        }
        BuildEvent::Done { green, iterations } => {
            eprintln!("[done] green={green} iterations={iterations}");
        }
        other => eprintln!("[event] {other:?}"),
    };
    let on_agent = |e: &sc_core::AgentEvent| eprintln!("  [agent] {e:?}");

    // One subtask that IS the fix goal, then the verify→fix loop drives `cargo check` → fixes.
    let tasks = [sc_workflow::BuildTask {
        id: "t1".into(),
        goal: "Fix the compile error in src/widgets.rs (a duplicated, unclosed function). Remove \
               the duplicate or balance the braces so `cargo check` passes."
            .into(),
        files: vec![file.clone()],
        deps: vec![],
    }];

    let outcome = sc_workflow::build_all_subtasks(
        &backend,
        &dir,
        &Sandbox::Host,
        "cargo check",
        &tasks,
        &on_event,
        &on_agent,
    );

    eprintln!(
        "OUTCOME: green={} iterations={} remaining={} | fix_passes={} checks={}",
        outcome.green,
        outcome.iterations,
        outcome.remaining.len(),
        fixes.load(Ordering::Relaxed),
        checks.load(Ordering::Relaxed),
    );

    // (1) TERMINATION — the whole point. The loop is bounded (MAX_ITERATIONS=8); with stall
    // detection it bails at ~2 non-improving rounds. Either way `iterations` is small and finite.
    assert!(
        outcome.iterations <= 8,
        "fix loop must be bounded, ran {} iterations",
        outcome.iterations
    );

    // (2) OUTCOME is coherent: green ⇒ no remaining errors; not green ⇒ it gave up with the errors
    // reported (never an infinite loop, never a false green). A capable model fixes it (green=true);
    // a weaker one bails cleanly. Both are acceptable — what's NOT acceptable is looping forever.
    if outcome.green {
        assert!(outcome.remaining.is_empty(), "green but errors remain?");
        // Prove it for real: cargo check now passes.
        let post = std::process::Command::new("cargo")
            .arg("check")
            .current_dir(&dir)
            .output()
            .expect("run cargo check");
        assert!(post.status.success(), "reported green but cargo check fails");
        eprintln!("✓ model fixed the delimiter cascade");
    } else {
        assert!(
            !outcome.remaining.is_empty(),
            "not green must report the remaining errors"
        );
        eprintln!("✓ model bailed cleanly (bounded), did not loop");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
