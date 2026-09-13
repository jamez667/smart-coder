//! Verify-command assembly, and the diagnostic for when one couldn't run.

use crate::config::UiConfig;

/// A human hint for why a verify command produced no parseable result — almost always the sandbox
/// can't run it (a Rust `cargo` build in the default Python `smart-coder-pyenv` image). Names the
/// command, the sandbox, and the concrete fix. Kept small + pure (takes what it needs) so it's
/// testable without a live run.
pub fn sandbox_verify_hint(cfg: &UiConfig, verify: &str, workspace: &std::path::Path) -> String {
    let sandbox = match cfg.sandbox() {
        sc_verify::Sandbox::Host => "the host".to_string(),
        sc_verify::Sandbox::Docker { image } => format!("the `{image}` container"),
        sc_verify::Sandbox::Session(c) => format!("the `{}` container", c.name()),
    };
    let is_rust =
        workspace.join("Cargo.toml").is_file() || verify.trim_start().starts_with("cargo");
    let uses_pyenv =
        matches!(cfg.sandbox(), sc_verify::Sandbox::Docker { image } if image.contains("pyenv"));
    if is_rust && uses_pyenv {
        format!(
            "`{verify}` can't run in {sandbox} (a Python image has no cargo). Set a Rust image \
             (SC_DOCKER_IMAGE=rust) or run on the host (SC_USE_DOCKER=0), then rebuild."
        )
    } else {
        format!(
            "`{verify}` exited non-zero with no diagnostics in {sandbox} — check it runs there."
        )
    }
}

/// One verify command that runs every test language present in `test_files`: pytest for
/// `.py` tests, vitest for `*.test.js`. Joined with `&&` so the gate is green only when
/// both pass. (The single agent loop has one verify command; this lets it cover a mixed
/// Python-backend + JS-frontend project.)
pub fn combined_verify_command(test_files: &[String]) -> String {
    let py: Vec<&String> = test_files.iter().filter(|f| f.ends_with(".py")).collect();
    let js: Vec<&String> = test_files
        .iter()
        .filter(|f| f.ends_with(".test.js"))
        .collect();
    let mut parts = Vec::new();
    if !py.is_empty() {
        // Name the frozen test files explicitly so pytest verifies the CONTRACT, not
        // whatever `test_*.py` happens to sit in the workspace (a stale file from a
        // prior run, or a scratch test the model wrote, must never poison the gate).
        let files = py
            .iter()
            .map(|f| shell_quote(f))
            .collect::<Vec<_>>()
            .join(" ");
        parts.push(format!("python -m pytest -q {files}"));
    }
    if !js.is_empty() {
        let files = js
            .iter()
            .map(|f| shell_quote(f))
            .collect::<Vec<_>>()
            .join(" ");
        parts.push(format!("vitest run {files}"));
    }
    if parts.is_empty() {
        "python -m pytest -q".to_string()
    } else {
        parts.join(" && ")
    }
}

/// Minimal POSIX single-quote (the sandbox runs the command via `sh -c`). Test paths
/// are workspace-relative and tame, but quoting keeps a path with spaces safe.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
