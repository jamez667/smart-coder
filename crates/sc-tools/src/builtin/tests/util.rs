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
