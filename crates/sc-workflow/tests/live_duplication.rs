//! Live reproduction of the CODE-DUPLICATION failure: the coder model, asked to add a helper to a
//! large existing file, tends to paste a block that already exists (or append a near-duplicate),
//! producing a redefinition / delimiter cascade that breaks the build (observed live 2026-07-21:
//! `widgets.rs` got a 227-line duplicate of its modal primitives).
//!
//! This is a LIVE test — it drives the real local model, whose behaviour varies run to run. So it
//! doesn't assert "duplicates every time"; it runs the scoped edit up to N times and asserts the
//! failure REPRODUCES at least once (proving it's a real, reproducible mode, not a one-off), while
//! logging the rate. When a dedup/edit fix lands, flip `EXPECT_DUPLICATION` to false: the same test
//! then asserts the model NEVER duplicates — turning the repro into a regression guard.
//!
//! Ignored by default (needs a live backend):
//!   cargo test -p sc-workflow --test live_duplication -- --ignored --nocapture

use std::path::Path;

use sc_core::{default_registry, run_agent_observed, select_strategy, AgentConfig, FnSink};
use sc_model::{ModelBackend, OpenAiBackend};
use sc_verify::Sandbox;

/// How many attempts to make; live behaviour is stochastic, so we sample.
const ATTEMPTS: usize = 5;
/// The dedup guard (sc-tools: `duplicate_definition`) has landed — the coder can no longer land a
/// duplicated top-level definition, so this is now a REGRESSION GUARD: every attempt must stay
/// duplicate-free. (Was `true` while reproducing the bug; flipped false once the fix was verified —
/// 5/5 attempts went duplicated=false compiles=true.)
const EXPECT_DUPLICATION: bool = false;

fn local_backend() -> OpenAiBackend {
    let base = std::env::var("SC_BASE_URL").unwrap_or_else(|_| "http://localhost:11435/v1".into());
    let model = std::env::var("SC_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    OpenAiBackend::new(base, model).with_detected_context()
}

/// A realistic, moderately-large `widgets.rs` that ALREADY defines `draw_modal_shell` and friends —
/// the exact shape that got duplicated. The file compiles as-is; the trap is asking the model to
/// "add" a helper that already exists, so a naive append/rewrite duplicates it. Returns the file's
/// workspace-relative path.
fn scaffold_widgets_crate(dir: &Path) -> String {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"widgets_dup\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub mod widgets;\n").unwrap();

    // A file with several small helpers, INCLUDING draw_modal_shell — long enough that the model
    // pins it and is tempted to re-emit a block rather than make a surgical edit.
    let widgets = "\
//! UI widget primitives (shared across panels).

pub struct Rect { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }

impl Rect {
    pub fn center(&self) -> (f32, f32) { (self.x + self.w * 0.5, self.y + self.h * 0.5) }
    pub fn area(&self) -> f32 { self.w * self.h }
}

/// Draw a full-screen dim backdrop behind a modal.
pub fn draw_backdrop(alpha: f32) -> f32 {
    // (rendering elided) — returns the alpha it used.
    alpha.clamp(0.0, 1.0)
}

/// Standard centred modal chrome: backdrop + panel + title. Returns the inner content rect.
pub fn draw_modal_shell(title: &str, size: Rect, backdrop_alpha: f32) -> Rect {
    let _ = draw_backdrop(backdrop_alpha);
    let _ = title.len();
    Rect { x: size.x + 8.0, y: size.y + 32.0, w: size.w - 16.0, h: size.h - 40.0 }
}

/// A labelled slider row.
pub fn draw_slider_row(label: &str, value: f32) -> (bool, f32) {
    let _ = label;
    (false, value)
}

/// A labelled button row. Returns whether it was clicked.
pub fn draw_button_row(label: &str) -> bool {
    !label.is_empty()
}
";
    std::fs::write(dir.join("src/widgets.rs"), widgets).unwrap();
    "src/widgets.rs".to_string()
}

/// Whether `src` defines ANY `pub fn` more than once — the duplication signal. The model tends to
/// re-emit existing functions (draw_slider_row, draw_button_row, …) when editing a file, not just
/// the one it was asked to add, so we detect any duplicated definition, not a single named one.
fn has_duplicate_fn(src: &str) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("pub fn ").or_else(|| t.strip_prefix("fn ")) {
            if let Some(name) = rest.split(['(', '<', ' ']).next().filter(|s| !s.is_empty()) {
                *counts.entry(name).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .find(|(_, n)| *n > 1)
        .map(|(name, n)| format!("{name} ×{n}"))
}

/// Whether cargo's stderr reports a "defined multiple times" (E0428) redefinition — the compile
/// symptom of the duplication, which also catches duplicated structs/impls, not just fns.
fn has_redefinition_error(stderr: &str) -> bool {
    stderr.contains("E0428") || stderr.contains("defined multiple times")
}

/// Run ONE scoped-edit attempt against a fresh copy of the crate: ask the model to add a helper
/// that ALREADY exists (the trap). Returns `(duplicated, compiles)` for this attempt.
fn one_attempt(backend: &OpenAiBackend, i: usize) -> (bool, bool) {
    let dir = std::env::temp_dir().join(format!("dc-live-dup-{}-{i}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = scaffold_widgets_crate(&dir);

    // The trap goal: it names draw_modal_shell as if it's new, in a file that already has it.
    let goal = format!(
        "Add a `draw_modal_shell` helper to `{file}` that draws a centred modal (backdrop + panel + \
         title) and returns the inner content rect. Wire it so other rows can call it. Then finish."
    );

    let registry = default_registry();
    let strategy = select_strategy(&backend.capabilities());
    let mut cfg = AgentConfig::default();
    cfg.focus_files = vec![file.clone()];
    cfg.sandbox = Sandbox::Host;
    cfg.verify_command = None;
    cfg.max_steps = 6;
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let _ = run_agent_observed(
        backend,
        None,
        &registry,
        strategy.as_ref(),
        &goal,
        &dir,
        &cfg,
        &sink,
    );

    let after = std::fs::read_to_string(dir.join(&file)).unwrap_or_default();
    let check = std::process::Command::new("cargo")
        .arg("check")
        .current_dir(&dir)
        .output()
        .expect("cargo check");
    let compiles = check.status.success();
    let stderr = String::from_utf8_lossy(&check.stderr);
    // Duplication is signalled two ways (either is a hit): a repeated `fn` definition in the source,
    // OR a redefinition (E0428) error from the compiler (also catches dup structs/impls).
    let dup_fn = has_duplicate_fn(&after);
    let duplicated = dup_fn.is_some() || has_redefinition_error(&stderr);
    eprintln!(
        "attempt {i}: duplicated={duplicated} compiles={compiles}{}",
        dup_fn
            .map(|d| format!("  (dup fn: {d})"))
            .unwrap_or_default()
    );
    if std::env::var("SC_DUP_DEBUG").is_ok() && duplicated {
        let first_errs: Vec<&str> = stderr
            .lines()
            .filter(|l| l.contains("error"))
            .take(3)
            .collect();
        eprintln!("  errors: {}", first_errs.join(" | "));
    }
    let _ = std::fs::remove_dir_all(&dir);
    (duplicated, compiles)
}

#[test]
#[ignore = "live: needs a local model backend + cargo on PATH"]
fn coder_duplicates_an_existing_helper_when_asked_to_add_it() {
    let backend = local_backend();

    let mut dup_count = 0usize;
    let mut broke_count = 0usize;
    for i in 0..ATTEMPTS {
        let (duplicated, compiles) = one_attempt(&backend, i);
        if duplicated {
            dup_count += 1;
        }
        if !compiles {
            broke_count += 1;
        }
    }
    eprintln!(
        "REPRO RATE: duplicated {dup_count}/{ATTEMPTS}, broke compile {broke_count}/{ATTEMPTS}"
    );

    if EXPECT_DUPLICATION {
        // The bug is REAL and reproducible: at least one attempt duplicated the existing helper.
        // (If this ever stops reproducing, the model or the harness improved — flip the flag.)
        assert!(
            dup_count > 0,
            "expected to reproduce the duplication at least once in {ATTEMPTS} attempts, but the \
             model never duplicated draw_modal_shell — has the bug been fixed? flip \
             EXPECT_DUPLICATION to false to make this a regression guard."
        );
    } else {
        // After the fix: the harness must NEVER let a duplicate land (edit dedup / surgical edits).
        assert_eq!(
            dup_count, 0,
            "regression: the coder duplicated draw_modal_shell in {dup_count}/{ATTEMPTS} attempts"
        );
    }
}
