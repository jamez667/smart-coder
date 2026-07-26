//! Where things live on disk: session logs, replay resolution, and the
//! auto-detected test-file oracle for a free-text `swarm` run.

/// Looks like a test file by the usual Python/pytest convention: `test_*.py`,
/// `*_test.py`, or anything under a `tests/` directory. Used to auto-freeze the
/// test oracle for a free-text `swarm <task>` run when `--frozen` wasn't given.
fn looks_like_test_file(rel: &str) -> bool {
    let norm = rel.replace('\\', "/");
    let name = norm.rsplit('/').next().unwrap_or(&norm);
    let is_py = name.ends_with(".py");
    let by_name = is_py && (name.starts_with("test_") || name.ends_with("_test.py"));
    let by_dir = norm.split('/').any(|seg| seg == "tests" || seg == "test");
    by_name || (by_dir && is_py)
}

/// Auto-detect the workspace's test files (one directory level deep plus a top-level
/// `tests/`), so a free-text `swarm` run gets the precise per-subtask scoped check
/// and test-oracle protection without the user listing files by hand (spec 08/11).
/// Best-effort: an unreadable workspace yields an empty list (the swarm then falls
/// back to the whole-suite-delta check, as before).
pub fn detect_test_files(workspace: &std::path::Path) -> Vec<String> {
    fn scan(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Recurse one level (and into any `tests/` dir) — deep trees are rare
                // for the small tasks the swarm targets, and we avoid walking the world.
                let name = entry.file_name();
                let is_tests = name.to_str() == Some("tests") || name.to_str() == Some("test");
                if depth == 0 || is_tests {
                    scan(&path, base, out, depth + 1);
                }
            } else if let Ok(rel) = path.strip_prefix(base) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if looks_like_test_file(&rel) && !out.contains(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    let mut out = Vec::new();
    scan(workspace, workspace, &mut out, 0);
    out.sort();
    out
}

/// Resolve where a run's session log is written (spec 06). An explicit `--log`
/// path wins; otherwise default to `<workspace>/.smart-coder/sessions/<id>.jsonl`,
/// where `<id>` is a millisecond timestamp — sortable, unique enough for one
/// user, and std-only (no extra crate). Returns the path and its session id.
pub fn session_log_path(
    workspace: &std::path::Path,
    log_override: Option<&str>,
) -> (std::path::PathBuf, String) {
    if let Some(p) = log_override {
        let path = std::path::PathBuf::from(p);
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        return (path, id);
    }
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "session".to_string());
    let path = sessions_dir(workspace).join(format!("{id}.jsonl"));
    (path, id)
}

/// Where session logs live: `<workspace>/.smart-coder/sessions/` — alongside the
/// planning workflow's `.smart-coder/plan/` (the dir is already gitignored).
pub fn sessions_dir(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join(".smart-coder").join("sessions")
}

/// Resolve a `replay` argument to a log file (spec 06): a path is used as-is; a
/// bare id resolves to `<workspace>/.smart-coder/sessions/<id>.jsonl`.
pub fn resolve_replay_path(workspace: &std::path::Path, session: &str) -> std::path::PathBuf {
    let direct = std::path::Path::new(session);
    if direct.is_file() {
        return direct.to_path_buf();
    }
    // A bare id (with or without the .jsonl suffix).
    let id = session.strip_suffix(".jsonl").unwrap_or(session);
    sessions_dir(workspace).join(format!("{id}.jsonl"))
}

#[cfg(test)]
mod private_tests {
    use super::looks_like_test_file;

    #[test]
    fn test_file_heuristic_matches_pytest_conventions() {
        assert!(looks_like_test_file("test_clamp.py"));
        assert!(looks_like_test_file("clamp_test.py"));
        assert!(looks_like_test_file("tests/anything.py"));
        assert!(looks_like_test_file("pkg/tests/util.py"));
        // Not tests.
        assert!(!looks_like_test_file("clamp.py"));
        assert!(!looks_like_test_file("contest.py")); // not test_/_test
        assert!(!looks_like_test_file("tests/data.json")); // under tests/ but not .py
    }
}
