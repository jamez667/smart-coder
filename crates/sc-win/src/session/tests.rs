//! Session tests: verify-command assembly, the git safety net, and the
//! spawn/stream contract.

use super::verify::{combined_verify_command, sandbox_verify_hint};
use super::{RunKind, Session, UiEvent};
use crate::config::UiConfig;
use sc_core::AgentEvent;

// The artifact-dir/slug rules now live in the engine (`sc_workflow::artifact_dir`),
// shared with the CLI, and are tested there.

#[test]
fn sandbox_verify_hint_flags_cargo_in_a_python_image() {
    // A Rust project (Cargo.toml present) + the default pyenv image ⇒ the hint names cargo,
    // the image, and the concrete fix. This is the "build incomplete, 0 errors" mystery.
    let dir = std::env::temp_dir().join(format!("dc-hint-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    let cfg = UiConfig {
        use_docker: true,
        docker_image: "smart-coder-pyenv".to_string(),
        sandbox_override: None,
        ..UiConfig::default()
    };
    let hint = sandbox_verify_hint(&cfg, "cargo check", &dir);
    assert!(hint.contains("cargo check"), "names the command: {hint}");
    assert!(
        hint.contains("smart-coder-pyenv"),
        "names the image: {hint}"
    );
    assert!(
        hint.contains("SC_DOCKER_IMAGE=rust") || hint.contains("SC_USE_DOCKER=0"),
        "gives the fix: {hint}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Unknown dirty state must revert NOTHING.**
///
/// `git_dirty_files` used to swallow every failure and return an empty set, which is
/// indistinguishable from a clean tree. The revert path partitions on
/// `!dirty.contains(f)`, so an unreadable git state classed EVERY touched file as
/// safe and `git checkout --` destroyed the user's own uncommitted work -- silently,
/// irreversibly, and reported as the harness tidying up after itself. An
/// `index.lock` held by a concurrent git (the GUI runs one) is enough to trigger it.
#[test]
fn an_unreadable_git_state_reverts_nothing() {
    // Not a git repo at all: `git status` fails, which is the shape of every other
    // failure too (lock contention, spawn error, non-zero exit).
    let dir = std::env::temp_dir().join(format!("sc-win-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mine.txt"), "MY UNCOMMITTED WORK\n").unwrap();

    let dirty = sc_iterate::git_dirty_files(&dir);
    assert!(
        dirty.is_none(),
        "an unreadable git state must be None, not empty"
    );

    // The caller's rule: with `None`, nothing is safe to revert.
    let touched = ["mine.txt".to_string()];
    let safe: Vec<String> = match dirty.as_ref() {
        Some(d) => touched
            .iter()
            .filter(|f| !d.contains(*f))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    assert!(
        safe.is_empty(),
        "unknown state must revert nothing, got {safe:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sandbox_verify_hint_generic_when_not_the_rust_pyenv_case() {
    // Host sandbox (or a non-Rust project) ⇒ a generic "check it runs there" message, not the
    // cargo/pyenv special-case.
    let dir = std::env::temp_dir().join(format!("dc-hint2-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir); // no Cargo.toml
    let cfg = UiConfig {
        use_docker: false, // host
        ..UiConfig::default()
    };
    let hint = sandbox_verify_hint(&cfg, "python -m pytest -q", &dir);
    assert!(hint.contains("the host"), "names host sandbox: {hint}");
    assert!(!hint.contains("cargo"), "no cargo special-case: {hint}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run a git command in `dir`, ignoring failures (test setup).
fn git(dir: &std::path::Path, args: &[&str]) {
    let _ = crate::proc::git().arg("-C").arg(dir).args(args).output();
}

#[test]
fn git_revert_restores_a_clean_file_and_dirty_detection_protects_uncommitted() {
    // Build a tiny real git repo to exercise the safety helpers end to end.
    let dir = std::env::temp_dir().join(format!("dc-git-safe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "committed-a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "committed-b\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "init"]);

    // Tree is clean now.
    assert!(
        sc_iterate::git_dirty_files(&dir)
            .expect("git is answerable here")
            .is_empty(),
        "clean after commit"
    );

    // User has uncommitted work in b.txt; a.txt is clean.
    std::fs::write(dir.join("b.txt"), "MY UNCOMMITTED WORK\n").unwrap();
    let dirty = sc_iterate::git_dirty_files(&dir).expect("git is answerable here");
    assert!(dirty.contains("b.txt"), "b.txt seen dirty: {dirty:?}");
    assert!(!dirty.contains("a.txt"), "a.txt still clean");

    // Simulate the agent breaking BOTH files.
    std::fs::write(dir.join("a.txt"), "BROKEN-a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "BROKEN-b\n").unwrap();

    // On failure we revert ONLY the file that was clean (a.txt), never b.txt.
    let touched = ["a.txt".to_string(), "b.txt".to_string()];
    let safe: Vec<String> = touched
        .iter()
        .filter(|f| !dirty.contains(*f))
        .cloned()
        .collect();
    assert_eq!(
        safe,
        vec!["a.txt".to_string()],
        "only the clean file is safe"
    );
    assert!(sc_iterate::git_revert_files(&dir, &safe));

    // a.txt restored to committed; b.txt's uncommitted work is UNTOUCHED (not reverted).
    // (Compare trimmed — git may normalize line endings on Windows checkout.)
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap().trim(),
        "committed-a",
        "clean file reverted to committed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap().trim(),
        "BROKEN-b",
        "dirty file left as-is (its uncommitted work not destroyed by a blind revert)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn git_helpers_are_safe_outside_a_repo() {
    let dir = std::env::temp_dir().join(format!("dc-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Not a git repo → dirty state UNKNOWN (not "empty"), revert reports false.
    // Empty would mean "nothing was dirty", which the caller reads as "everything is
    // safe to revert" -- see `an_unreadable_git_state_reverts_nothing`.
    assert!(sc_iterate::git_dirty_files(&dir).is_none());
    assert!(!sc_iterate::git_revert_files(&dir, &["x.txt".to_string()]));
    // Empty list is a no-op success.
    assert!(sc_iterate::git_revert_files(&dir, &[]));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A spawned agent run against an unreachable backend still streams a terminal
/// `UiEvent` (Failed) rather than hanging — the UI always learns the run ended.
#[test]
fn unreachable_backend_yields_a_terminal_event() {
    let cfg = UiConfig {
        // A port nothing listens on ⇒ the backend call errors fast.
        base_url: "http://127.0.0.1:1/v1".to_string(),
        model: "none".to_string(),
        ..UiConfig::default()
    };
    let ws = std::env::temp_dir();
    let session = Session::spawn(RunKind::Agent, cfg, "do a thing".to_string(), ws);

    // Block for the terminal event by polling the worker to completion.
    let terminal = wait_for_terminal(&session);
    assert!(
        matches!(
            terminal,
            Some(UiEvent::Failed(_)) | Some(UiEvent::Done { .. })
        ),
        "expected a terminal UiEvent, got {terminal:?}"
    );
}

/// Drain until a Done/Failed arrives (or the worker thread ends and the channel
/// closes). Test-only; the real UI drains per-frame.
fn wait_for_terminal(session: &Session) -> Option<UiEvent> {
    loop {
        match session.events.recv() {
            Ok(ev @ (UiEvent::Done { .. } | UiEvent::Failed(_))) => return Some(ev),
            Ok(_) => continue,     // intermediate event; keep waiting
            Err(_) => return None, // worker ended without a terminal (shouldn't happen)
        }
    }
}

#[test]
fn verify_command_targets_only_the_frozen_tests() {
    // The gate must name the frozen test files, not blanket-collect `test_*.py`
    // — a stale or scratch test in the workspace must never poison verification.
    let cmd =
        combined_verify_command(&["test_app.py".to_string(), "static/app.test.js".to_string()]);
    assert!(
        cmd.contains("pytest -q 'test_app.py'"),
        "pytest scoped to the frozen file: {cmd}"
    );
    assert!(
        cmd.contains("vitest run 'static/app.test.js'"),
        "vitest scoped to the frozen file: {cmd}"
    );
    // No bare whole-directory pytest.
    assert!(
        !cmd.contains("pytest -q &&") && !cmd.trim_end().ends_with("pytest -q"),
        "must not run an unscoped pytest: {cmd}"
    );
}

#[test]
fn py_only_verify_is_scoped() {
    let cmd = combined_verify_command(&["test_app.py".to_string()]);
    assert_eq!(cmd, "python -m pytest -q 'test_app.py'");
}

#[test]
fn ui_event_is_cloneable_for_the_iced_message() {
    // iced Messages must be Clone; UiEvent wraps the (Clone) core events.
    let e = UiEvent::Agent(AgentEvent::ToolCall {
        tool: "read_file".to_string(),
        arg: "src/main.rs".to_string(),
    });
    let _ = e.clone();
}
