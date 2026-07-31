//! The workspace helpers: the source-file ledger.

use super::temp_dir;
use crate::builtin::util::source_files;

#[test]
fn source_files_lists_real_files_excluding_tests_and_caches() {
    let ws = temp_dir("srcfiles");
    std::fs::create_dir_all(ws.join("templates")).unwrap();
    std::fs::create_dir_all(ws.join("static")).unwrap();
    std::fs::create_dir_all(ws.join("__pycache__")).unwrap();
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join("app.py"), "x").unwrap();
    std::fs::write(ws.join("templates/board.html"), "x").unwrap();
    std::fs::write(ws.join("static/board.js"), "x").unwrap();
    std::fs::write(ws.join("test_app.py"), "x").unwrap(); // frozen test → excluded
    std::fs::write(ws.join("__pycache__/app.pyc"), "x").unwrap(); // cache → excluded
    std::fs::write(ws.join(".git/config"), "x").unwrap(); // dot-dir → excluded

    let files = source_files(&ws);
    assert_eq!(
        files,
        vec![
            "app.py".to_string(),
            "static/board.js".to_string(),
            "templates/board.html".to_string(),
        ],
        "only real sources, sorted, '/'-sep; tests/cache/dot-dirs excluded"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn source_files_is_empty_for_a_fresh_dir() {
    let ws = temp_dir("srcfiles-empty");
    assert!(source_files(&ws).is_empty());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn source_files_excludes_the_workflows_own_artifacts() {
    // Observed live: a spec drafted against an empty repository listed
    // `lease.json` under "Files to Touch" — because the only file the survey
    // found was the lease the drafting run was itself holding. `specs/<slug>/`
    // is deliberately not hidden (the artifacts are meant to be reviewed as a
    // diff and committed), so it must be excluded by name instead.
    let ws = temp_dir("srcfiles-artifacts");
    std::fs::create_dir_all(ws.join("specs/add-a-health-check")).unwrap();
    std::fs::write(ws.join("app.py"), "x").unwrap();
    for artifact in [
        "state.json",
        "lease.json",
        "spec.md",
        "architecture.md",
        "layout.md",
        "breakdown.md",
        "decomposition.md",
    ] {
        std::fs::write(ws.join("specs/add-a-health-check").join(artifact), "x").unwrap();
    }
    // A hand-written design note under specs/ is still real source, though —
    // excluding the whole tree would hide documents a human wrote.
    std::fs::write(ws.join("specs/add-a-health-check/notes.md"), "x").unwrap();

    let files = source_files(&ws);
    assert_eq!(
        files,
        vec![
            "app.py".to_string(),
            "specs/add-a-health-check/notes.md".to_string(),
        ],
        "the run's own bookkeeping must not be surveyed as project source"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_artifact_name_outside_specs_is_still_source() {
    // A project's own `state.json` at the root is ordinary source. Matching on
    // the filename alone would hide it.
    let ws = temp_dir("srcfiles-rootstate");
    std::fs::write(ws.join("state.json"), "x").unwrap();
    assert_eq!(source_files(&ws), vec!["state.json".to_string()]);
    let _ = std::fs::remove_dir_all(&ws);
}
