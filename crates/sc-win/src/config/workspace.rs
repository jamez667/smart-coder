//! Where a run works, what it built, and how to verify it: the workspace defaults,
//! the source-file ledger, verify-command detection, and the repo overview.

/// The default GUI workspace: an isolated scratch dir under the system temp dir. This
/// is deliberately NOT the current/launch dir — a swarm writing whole files must never
/// land in the user's source tree.
pub fn default_workspace() -> std::path::PathBuf {
    std::env::temp_dir().join("smart-coder-workspace")
}

/// List the **source** files in `workspace` (workspace-relative, sorted) — i.e. the
/// real output, excluding tests, the plan dir, and tooling caches. This is what a run
/// actually *built*, so the UI can show "5 files built" / "0 files built" plainly.
pub fn source_files(workspace: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip hidden/dot dirs (.smart-coder, .pytest_cache, .git), caches, deps.
            if name.starts_with('.') || name == "__pycache__" || name == "node_modules" {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !is_test_file(&rel) {
                        out.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Whether a workspace-relative path looks like a test file (so it's excluded from the
/// "source files built" count).
fn is_test_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("test_")
        || lower.contains(".test.")
        || lower.contains("_test.")
        || lower.contains(".spec.")
}

/// Pick the verify command that matches the tests that were actually written, so a
/// JS/JSX project isn't checked with `pytest` (it wrote JS tests it can't run —
/// observed live 2026-06-14). Detects the dominant test language from the test file
/// extensions and returns the conventional runner. Falls back to `python -m pytest`
/// (the configured default) when nothing recognizable was written.
pub fn detect_verify_command(test_files: &[String], fallback: &str) -> String {
    let mut py = 0usize;
    let mut js = 0usize;
    let mut rs = 0usize;
    let mut go = 0usize;
    let mut cs = 0usize;
    for f in test_files {
        let lower = f.to_ascii_lowercase();
        if lower.ends_with(".py") {
            py += 1;
        } else if lower.ends_with(".js")
            || lower.ends_with(".jsx")
            || lower.ends_with(".ts")
            || lower.ends_with(".tsx")
        {
            js += 1;
        } else if lower.ends_with(".rs") {
            rs += 1;
        } else if lower.ends_with(".cs") {
            cs += 1;
        } else if lower.ends_with("_test.go") || lower.ends_with(".go") {
            go += 1;
        }
    }
    // Pick the language with the most test files; ties favour the fallback's spirit.
    let max = py.max(js).max(rs).max(go).max(cs);
    if max == 0 {
        return fallback.to_string();
    }
    if js == max {
        // Vitest runs jest-style tests and is the lightest to invoke headlessly.
        "npx vitest run".to_string()
    } else if py == max {
        "python -m pytest -q".to_string()
    } else if rs == max {
        "cargo test".to_string()
    } else if cs == max {
        // A standalone C# test project; a real Unity project resolves the Editor batchmode
        // gate in `iterate_verify_command` (which has the workspace path).
        "dotnet test".to_string()
    } else {
        "go test ./...".to_string()
    }
}

/// Build a short overview of the files already in `workspace`, for the decomposer's
/// `repo_overview` — so when iterating on an existing project the orchestrator plans
/// *edits to existing files* (and new files) instead of assuming a blank slate. Returns
/// an empty string for an empty/missing dir (the from-scratch case). Walks recursively,
/// listing workspace-relative paths with byte sizes; capped so a huge tree can't blow
/// the prompt budget.
pub fn repo_overview(workspace: &std::path::Path) -> String {
    /// Cap on listed files (keep the decomposer prompt bounded).
    const MAX_FILES: usize = 200;

    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip VCS/build/generated noise so the overview is the user's actual sources
            // (and doesn't get swamped by e.g. a `screenshots/` folder full of PNGs).
            if crate::filetree::is_noise_dir(name) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let rel = path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push((rel, size));
                }
                _ => {}
            }
        }
    }

    if files.is_empty() {
        return String::new();
    }
    files.sort();
    let truncated = files.len() > MAX_FILES;
    let mut out = String::from("Existing files (edit these in place where the task applies):\n");
    for (rel, size) in files.iter().take(MAX_FILES) {
        out.push_str(&format!("  {rel} ({size} bytes)\n"));
    }
    if truncated {
        out.push_str(&format!("  … and {} more\n", files.len() - MAX_FILES));
    }
    out
}
