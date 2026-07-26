//! Session-log placement, replay resolution, and test-file auto-detection.

use crate::{detect_test_files, resolve_replay_path, session_log_path};

#[test]
fn detect_test_files_finds_tests_one_level_and_in_tests_dir() {
    let ws = std::env::temp_dir().join(format!(
        "sc-cli-detect-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(ws.join("tests")).unwrap();
    std::fs::write(ws.join("clamp.py"), "x=1\n").unwrap();
    std::fs::write(ws.join("test_clamp.py"), "x=1\n").unwrap();
    std::fs::write(ws.join("tests").join("test_more.py"), "x=1\n").unwrap();
    std::fs::write(ws.join("tests").join("helpers.py"), "x=1\n").unwrap(); // under tests/

    let mut found = detect_test_files(&ws);
    found.sort();
    assert_eq!(
        found,
        vec!["test_clamp.py", "tests/helpers.py", "tests/test_more.py"],
        "should freeze test_*.py and everything under tests/, not clamp.py"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn session_log_path_defaults_under_dot_dir_and_honors_override() {
    let ws = std::path::Path::new("/tmp/ws");
    // Default: .smart-coder/sessions/<millis>.jsonl, id is the numeric stem.
    let (path, id) = session_log_path(ws, None);
    assert!(path.ends_with(format!("{id}.jsonl")), "{path:?}");
    assert!(path.to_string_lossy().contains("sessions"), "{path:?}");
    assert!(id.chars().all(|c| c.is_ascii_digit()), "id={id}");
    // Override wins; id is derived from the file stem.
    let (p2, id2) = session_log_path(ws, Some("logs/my-run.jsonl"));
    assert_eq!(p2, std::path::PathBuf::from("logs/my-run.jsonl"));
    assert_eq!(id2, "my-run");
}

#[test]
fn resolve_replay_path_handles_bare_id_and_suffix() {
    let ws = std::path::Path::new("/tmp/ws");
    let from_id = resolve_replay_path(ws, "123");
    assert!(from_id.ends_with("sessions/123.jsonl") || from_id.ends_with("sessions\\123.jsonl"));
    // A .jsonl-suffixed bare id resolves to the same place (not doubled).
    let from_suffixed = resolve_replay_path(ws, "123.jsonl");
    assert_eq!(from_id, from_suffixed);
}
