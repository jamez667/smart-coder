//! Sequential-build tests: slicing, ordering, and the per-file/integration contracts.

use super::build::*;
use super::pass::*;
use super::report::*;
use super::slice::*;
use sc_core::AgentConfig;
use sc_core::FnSink;
use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use sc_proto::Result as DcResult;
use sc_swarm::Subtask;
use sc_swarm::TaskBoard;
use std::cell::RefCell;
use std::sync::Mutex;

/// A backend that records every instruction it was asked to act on, and replays a fixed
/// reply (default: write the file named in the instruction, then finish — so per-file
/// steps "succeed" deterministically without a real model).
struct SpyBackend {
    seen_instructions: Mutex<Vec<String>>,
    // Each call: emit a write_file for the FIRST `path` the instruction names + finish.
    script: RefCell<Vec<String>>,
}
impl SpyBackend {
    fn new() -> Self {
        Self {
            seen_instructions: Mutex::new(Vec::new()),
            script: RefCell::new(Vec::new()),
        }
    }
}
impl ModelBackend for SpyBackend {
    fn name(&self) -> &str {
        "spy"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, req: &GenerateRequest) -> DcResult<GenerateResponse> {
        // The user message carries the instruction; record it once per turn.
        let instr = req
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.seen_instructions.lock().unwrap().push(instr.clone());
        // If the instruction names a file to write, write it then finish. Else finish.
        // Parse the backtick-quoted `path` from "Write ONLY the file `x`".
        let path = instr
            .split("Write ONLY the file `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .map(|s| s.to_string());
        let content = match path {
            Some(p) if !self.script.borrow().contains(&p) => {
                self.script.borrow_mut().push(p.clone());
                format!("{{\"tool\":\"write_file\",\"path\":\"{p}\",\"content\":\"# {p}\\n\"}}")
            }
            _ => "{\"tool\":\"finish\"}".to_string(),
        };
        Ok(GenerateResponse::new(content))
    }
}

fn ws(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dc-wf-seq-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn base_cfg() -> AgentConfig {
    AgentConfig {
        // No verify command at the base → the final pass is ungated too (verified=None),
        // which is fine for these structural tests (no Docker, we assert ordering/scoping).
        verify_command: None,
        ..AgentConfig::default()
    }
}

#[test]
fn walks_the_board_in_dependency_order_one_file_each() {
    let dir = ws("order");
    let board = TaskBoard::new(vec![
        Subtask::new("t3", "build c")
            .with_files(vec!["c.py".into()])
            .with_deps(vec!["t1".into(), "t2".into()]),
        Subtask::new("t1", "build a").with_files(vec!["a.py".into()]),
        Subtask::new("t2", "build b")
            .with_files(vec!["b.py".into()])
            .with_deps(vec!["t1".into()]),
    ]);
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let rep =
        build_sequential_with_board(board, &spy, "task", &dir, &base_cfg(), 1, &sink).unwrap();

    assert!(!rep.fell_back_whole_task);
    // Per-file steps ran in dep order a → b → c.
    let order: Vec<&str> = rep.per_file.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(order, vec!["t1", "t2", "t3"], "dep order");
    // Each per-file instruction named exactly its own file.
    let seen = spy.seen_instructions.lock().unwrap();
    assert!(seen.iter().any(|i| i.contains("`a.py`")));
    assert!(seen.iter().any(|i| i.contains("`b.py`")));
    assert!(seen.iter().any(|i| i.contains("`c.py`")));
    // The files were actually written to disk by the per-file steps.
    assert!(dir.join("a.py").exists() && dir.join("b.py").exists() && dir.join("c.py").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn degenerate_board_falls_back_to_whole_task() {
    let dir = ws("degen");
    // Single subtask with NO files = the documented decomposition fallback.
    let board = TaskBoard::new(vec![Subtask::new("t1", "do the whole thing")]);
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let rep = build_sequential_with_board(board, &spy, "whole task", &dir, &base_cfg(), 1, &sink)
        .unwrap();
    assert!(
        rep.fell_back_whole_task,
        "degenerate board → whole-task fallback"
    );
    assert!(rep.per_file.is_empty(), "no per-file steps in fallback");
    // The whole-task instruction (not a per-file one) was used.
    let seen = spy.seen_instructions.lock().unwrap();
    assert!(seen.iter().any(|i| i.contains("Implement this project")));
    assert!(!seen.iter().any(|i| i.contains("Write ONLY the file")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_board_falls_back_and_terminates() {
    let dir = ws("empty");
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let rep = build_sequential_with_board(
        TaskBoard::new(vec![]),
        &spy,
        "t",
        &dir,
        &base_cfg(),
        1,
        &sink,
    )
    .unwrap();
    assert!(rep.fell_back_whole_task);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dependency_cycle_still_terminates() {
    let dir = ws("cycle");
    // t1 ↔ t2 mutual deps: ready() is always empty, but the lowest-pending guard must
    // run them anyway so the loop terminates rather than hanging.
    let board = TaskBoard::new(vec![
        Subtask::new("t1", "a")
            .with_files(vec!["a.py".into()])
            .with_deps(vec!["t2".into()]),
        Subtask::new("t2", "b")
            .with_files(vec!["b.py".into()])
            .with_deps(vec!["t1".into()]),
    ]);
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let rep = build_sequential_with_board(board, &spy, "t", &dir, &base_cfg(), 1, &sink).unwrap();
    // Both subtasks were attempted (≤ len iterations, no hang).
    assert_eq!(rep.per_file.len(), 2, "both attempted via the guard");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_file_instruction_embeds_contract_and_drops_ignore_framing() {
    // The fix: the per-file step must SEE the contract and must NOT be told to ignore it.
    let s = per_file_instruction(
        &["store.py".into()],
        "build the store",
        "def test_save(): assert save('u') is not None",
    );
    assert!(
        s.contains("`store.py`"),
        "names the file (SpyBackend parse): {s}"
    );
    assert!(
        s.contains("assert save('u')"),
        "embeds the contract asserts"
    );
    assert!(s.contains("FROZEN"), "frames the tests as the contract");
    assert!(
        !s.contains("NOT your concern") && !s.contains("no tests to run"),
        "the old ignore-the-tests framing must be gone: {s}"
    );
    // With no contract, no fenced block (degenerate/missing-test case).
    let bare = per_file_instruction(&["a.py".into()], "g", "");
    assert!(bare.contains("`a.py`") && !bare.contains("```python"));
}

#[test]
fn per_file_steps_see_the_frozen_contract_from_disk() {
    // End-to-end through the driver: a test_app.py on disk reaches the per-file prompt
    // (via the glob fallback — base_cfg here has no frozen_paths).
    let dir = ws("contract");
    std::fs::write(
        dir.join("test_app.py"),
        "def test_save_returns_code():\n    assert save('u') is not None\n",
    )
    .unwrap();
    let board = TaskBoard::new(vec![
        Subtask::new("t1", "build store").with_files(vec!["store.py".into()])
    ]);
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    build_sequential_with_board(board, &spy, "task", &dir, &base_cfg(), 1, &sink).unwrap();
    let seen = spy.seen_instructions.lock().unwrap();
    assert!(
        seen.iter()
            .any(|i| i.contains("`store.py`") && i.contains("test_save_returns_code")),
        "the per-file prompt must carry the on-disk test contract"
    );
    assert!(!seen.iter().any(|i| i.contains("NOT your concern")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_frozen_contract_prefers_frozen_paths_then_globs() {
    let dir = ws("frozen-read");
    std::fs::write(dir.join("test_app.py"), "A").unwrap();
    std::fs::write(dir.join("test_more.py"), "B").unwrap();
    // Explicit frozen_paths win and are read in that order.
    let explicit = read_frozen_contract(&dir, &["test_app.py".to_string()]);
    assert_eq!(explicit, "A");
    // No frozen_paths → glob test_*.py (sorted): A then B.
    let globbed = read_frozen_contract(&dir, &[]);
    assert!(globbed.contains("A") && globbed.contains("B"));
    // Missing dir / no tests → empty.
    assert_eq!(read_frozen_contract(&ws("frozen-empty"), &[]), "");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_file_registry_has_write_but_no_verification() {
    // The per-file registry must let a step CREATE a file (write_file) but NOT have
    // run_verification (which dead-ends on the intentionally-absent verify command).
    let names: Vec<&str> = per_file_registry().specs().iter().map(|s| s.name).collect();
    assert!(
        names.contains(&"write_file"),
        "needs write_file to create files"
    );
    assert!(names.contains(&"edit_file"));
    assert!(names.contains(&"finish"));
    assert!(
        !names.contains(&"run_verification"),
        "must NOT have run_verification (the dead-end that stalled per-file steps)"
    );
    assert!(!names.contains(&"run_command"));
}

#[test]
fn feature_keyword_maps_route_files_and_skips_infra() {
    assert_eq!(
        feature_keyword("routes_authors.py").as_deref(),
        Some("author")
    );
    assert_eq!(feature_keyword("routes_books.py").as_deref(), Some("book"));
    assert_eq!(feature_keyword("routes_loans.py").as_deref(), Some("loan"));
    // Infrastructure + glue are not their own feature slice.
    for f in [
        "store.py",
        "service.py",
        "app.py",
        "templates/catalog.html",
        "static/catalog.js",
        "routes.py",
    ] {
        assert_eq!(feature_keyword(f), None, "{f} should not be a slice");
    }
}

#[test]
fn parse_test_names_extracts_def_test_lines() {
    let contract = "from app import app\n\ndef c():\n    return app\n\ndef test_create_author_and_book():\n    pass\n\ndef test_loan_book_ok():\n    pass\ndef test_catalog_page():\n    pass\n";
    let names = parse_test_names(contract);
    assert_eq!(
        names,
        vec![
            "test_create_author_and_book",
            "test_loan_book_ok",
            "test_catalog_page"
        ]
    );
    // `def c()` (not a test) is excluded.
    assert!(!names.iter().any(|n| n == "c"));
}

#[test]
fn derive_slices_yields_features_in_dep_order_with_tests() {
    // An S3-shaped board: store→service→routes_authors→routes_books→routes_loans→app→template.
    let board = TaskBoard::new(vec![
        Subtask::new("t1", "store").with_files(vec!["store.py".into()]),
        Subtask::new("t2", "service")
            .with_files(vec!["service.py".into()])
            .with_deps(vec!["t1".into()]),
        Subtask::new("t3", "authors")
            .with_files(vec!["routes_authors.py".into()])
            .with_deps(vec!["t2".into()]),
        Subtask::new("t4", "books")
            .with_files(vec!["routes_books.py".into()])
            .with_deps(vec!["t3".into()]),
        Subtask::new("t5", "loans")
            .with_files(vec!["routes_loans.py".into()])
            .with_deps(vec!["t4".into()]),
        Subtask::new("t6", "app")
            .with_files(vec!["app.py".into()])
            .with_deps(vec!["t5".into()]),
    ]);
    let names: Vec<String> = [
        "test_create_author",
        "test_book_requires_author",
        "test_loan_book_ok",
        "test_catalog_page",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let slices = derive_slices(&board, &names);
    let kws: Vec<&str> = slices.iter().map(|s| s.keyword.as_str()).collect();
    // author/book/loan in dep order; app/store/service yield no slice; "catalog" has a test
    // but no routes_catalog.py, so no slice (the final full pass catches it).
    assert_eq!(kws, vec!["author", "book", "loan"]);
}

#[test]
fn derive_slices_empty_when_no_route_files() {
    // A single-routes.py app (S1/S2 shape) → no feature slices → caller falls back.
    let board = TaskBoard::new(vec![
        Subtask::new("t1", "store").with_files(vec!["store.py".into()]),
        Subtask::new("t2", "app")
            .with_files(vec!["app.py".into()])
            .with_deps(vec!["t1".into()]),
    ]);
    let names = vec!["test_create".to_string(), "test_resolve".to_string()];
    assert!(derive_slices(&board, &names).is_empty());
}

#[test]
fn incremental_integration_is_bounded_when_no_slice_ever_converges() {
    // The convergence-oscillation guard: even if EVERY slice stays red (the model never makes
    // it green — the "flat at 9-failed for 80 cycles" shape), the integration loop must run at
    // most ONE pass per slice and terminate — it's a bounded `for`, not a `while green`. This
    // proves it can't spin regardless of model behaviour, with no live backend.
    let dir = ws("incr-bound");
    // Two feature slices worth of files, so `derive_slices` yields 2 slices.
    std::fs::write(dir.join("routes_authors.py"), "# authors\n").unwrap();
    std::fs::write(dir.join("routes_books.py"), "# books\n").unwrap();
    let slices = vec![
        FeatureSlice {
            keyword: "author".into(),
            file: "routes_authors.py".into(),
        },
        FeatureSlice {
            keyword: "book".into(),
            file: "routes_books.py".into(),
        },
    ];

    // A verify command that ALWAYS fails on the host, so no slice ever goes green (the pre-check
    // never short-circuits and the agent pass never satisfies the gate). `exit 1` needs no
    // interpreter — portable across `cmd /C` (Windows) and `sh -c` (Unix).
    let mut cfg = base_cfg();
    cfg.sandbox = sc_verify::Sandbox::Host;
    cfg.verify_command = Some("exit 1".to_string());

    let backend = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});

    let steps =
        run_incremental_integration(&backend, "build it", &dir, &cfg, &slices, &sink).unwrap();

    // Exactly one agent step per slice — bounded, terminated. Never more (no spinning), and it
    // returned Ok rather than hanging.
    assert_eq!(
        steps.len(),
        slices.len(),
        "one bounded pass per slice, got {}",
        steps.len()
    );
    // Each step is labelled for its slice (author, then author+book) — dependency order kept.
    assert!(steps[0].0.contains("author"));
    assert!(steps[1].0.contains("book"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cumulative_k_and_slice_command_build_growing_filters() {
    let slices = vec![
        FeatureSlice {
            keyword: "author".into(),
            file: "routes_authors.py".into(),
        },
        FeatureSlice {
            keyword: "book".into(),
            file: "routes_books.py".into(),
        },
        FeatureSlice {
            keyword: "loan".into(),
            file: "routes_loans.py".into(),
        },
    ];
    assert_eq!(cumulative_k(&slices, 0), "author");
    assert_eq!(cumulative_k(&slices, 1), "author or book");
    assert_eq!(cumulative_k(&slices, 2), "author or book or loan");
    assert_eq!(
        slice_command("python -m pytest -q 'test_app.py'", "author or book"),
        "python -m pytest -q 'test_app.py' -k \"author or book\""
    );
}

#[test]
fn incremental_integration_runs_slices_in_order_then_full_pass() {
    // With route files + matching tests + a verify command, the driver runs each cumulative
    // slice (author, author or book, author or book or loan) THEN the full pass. We use an
    // always-failing verify command so each slice pre-check is red (the agent loop runs and
    // records its sliced instruction) — we assert the ORDER of instructions, not greenness.
    let dir = ws("incr");
    let board = TaskBoard::new(vec![
        Subtask::new("t1", "authors").with_files(vec!["routes_authors.py".into()]),
        Subtask::new("t2", "books")
            .with_files(vec!["routes_books.py".into()])
            .with_deps(vec!["t1".into()]),
        Subtask::new("t3", "loans")
            .with_files(vec!["routes_loans.py".into()])
            .with_deps(vec!["t2".into()]),
    ]);
    // Frozen contract drives parse_test_names; write it so read_frozen_contract finds it.
    std::fs::write(
        dir.join("test_app.py"),
        "def test_author():\n    pass\ndef test_book():\n    pass\ndef test_loan():\n    pass\n",
    )
    .unwrap();
    let mut cfg = AgentConfig {
        // An unknown program → shell exits non-zero → not all_green → each slice loop runs.
        verify_command: Some("dc_nonexistent_verify_cmd_xyz".to_string()),
        ..AgentConfig::default()
    };
    cfg.permission.frozen_paths = vec!["test_app.py".to_string()];
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let rep = build_sequential_with_board(board, &spy, "lib", &dir, &cfg, 1, &sink).unwrap();

    // The incremental steps were recorded in cumulative order.
    let labels: Vec<&str> = rep.incremental.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "slice:author",
            "slice:author or book",
            "slice:author or book or loan"
        ]
    );
    // The model saw the sliced instructions in order, then the full-suite pass last.
    let seen = spy.seen_instructions.lock().unwrap();
    let slice_positions: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, i)| i.contains("GROWING SLICE"))
        .map(|(n, _)| n)
        .collect();
    assert!(slice_positions.len() >= 3, "ran the slice loops");
    let last = seen.last().unwrap();
    assert!(
        last.contains("Make the FULL frozen test suite pass"),
        "full pass is last: {last}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn final_pass_runs_unfocused_after_the_per_file_steps() {
    let dir = ws("final");
    let board = TaskBoard::new(vec![Subtask::new("t1", "a").with_files(vec!["a.py".into()])]);
    let spy = SpyBackend::new();
    let sink = FnSink(|_e: &sc_core::AgentEvent| {});
    let _ =
        build_sequential_with_board(board, &spy, "the task", &dir, &base_cfg(), 1, &sink).unwrap();
    // The LAST instruction the model saw is the integration pass, not a per-file one.
    let seen = spy.seen_instructions.lock().unwrap();
    let last = seen.last().unwrap();
    assert!(
        last.contains("Make the FULL frozen test suite pass"),
        "final pass is the integration instruction: {last}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
