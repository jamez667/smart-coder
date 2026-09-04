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
///
/// This was a hand-synced copy of `sc_tools::source_files`, kept "in sync deliberately"
/// — which is to say, kept in sync by remembering to. It now calls it (spec 23: one
/// walk, one skip list). The one real difference the copy had is preserved by that
/// function: it also drops the workflow's own artifacts, which are not project source
/// either.
pub fn source_files(workspace: &std::path::Path) -> Vec<String> {
    sc_tools::source_files(workspace)
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

/// Build a short overview of the files already in `workspace`, for the decomposer.
///
/// Re-exported from `sc_iterate` rather than kept as a second copy. This was a
/// line-for-line fork -- same walk, same MAX_FILES cap, same noise filter -- and
/// both build PROMPT TEXT, so a divergence changes what the model is told between
/// the desktop and the remote server. `sc-iterate` exists specifically to keep
/// those two behaving identically.
pub use sc_iterate::repo_overview;
