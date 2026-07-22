//! Language-aware verify-command detection for iterate mode. Picks a sensible gate
//! (`cargo check`, `npm run build`, Unity EditMode tests) when the user hasn't set a
//! meaningful explicit command, so a small model's edits are actually checked.

use std::path::Path;

/// The pytest default the from-scratch build ships with. In iterate mode this is a
/// poor gate (existing projects are usually not fresh Python apps), so we treat
/// leaving it as "unset" and pick a language-appropriate default instead.
const PYTEST_DEFAULT: &str = "python -m pytest -q";

/// Choose the verify command for an iterate run. Honor an explicit non-default
/// command; otherwise detect the workspace language: Unity → EditMode tests,
/// Rust → `cargo check`, Node → `npm run build --if-present`, else keep configured.
pub fn iterate_verify_command(configured: &Option<String>, workspace: &Path) -> Option<String> {
    if let Some(cmd) = configured {
        let c = cmd.trim();
        if !c.is_empty() && c != PYTEST_DEFAULT {
            return Some(c.to_string());
        }
    }
    if workspace.join("Assets").is_dir() && workspace.join("ProjectSettings").is_dir() {
        let cmd = unity_verify_command(workspace);
        return if cmd.is_empty() {
            configured.clone()
        } else {
            Some(cmd)
        };
    }
    if workspace.join("Cargo.toml").is_file() {
        return Some("cargo check".to_string());
    }
    if workspace.join("package.json").is_file() {
        return Some("npm run build --if-present".to_string());
    }
    // A Python project (has .py files or a pytest/setup config) → run pytest, so the
    // agent can actually VERIFY its fix instead of being stuck (no verify command means
    // it tries shell, gets blocked, and loops until the step budget).
    if is_python_project(workspace) {
        return Some(PYTEST_DEFAULT.to_string());
    }
    configured.clone()
}

/// The Unity verify gate: the Editor batchmode CLI running EditMode tests; degrades to
/// `dotnet build` when a solution exists, else empty (caller keeps configured).
fn unity_verify_command(workspace: &Path) -> String {
    match find_unity_editor() {
        Some(editor) => {
            let ws = workspace.display();
            let results = workspace.join("Temp").join("sc-tests.xml");
            format!(
                "\"{}\" -batchmode -quit -projectPath \"{ws}\" -runTests -testPlatform EditMode \
                 -testResults \"{}\" -logFile -",
                editor.display(),
                results.display(),
            )
        }
        None => {
            if has_dotnet_project(workspace) {
                "dotnet build".to_string()
            } else {
                String::new()
            }
        }
    }
}

/// Resolve a Unity Editor executable: `SC_UNITY_EDITOR` env override, then common Hub
/// install locations, then falls through to nothing.
fn find_unity_editor() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("SC_UNITY_EDITOR") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    #[cfg(target_os = "windows")]
    let hub_roots = [
        std::path::PathBuf::from(r"C:\Program Files\Unity\Hub\Editor"),
        std::path::PathBuf::from(r"C:\Program Files\Unity\Editor"),
    ];
    #[cfg(not(target_os = "windows"))]
    let hub_roots = [
        std::path::PathBuf::from("/Applications/Unity/Hub/Editor"),
        std::path::PathBuf::from("/opt/unity/editors"),
    ];
    for root in hub_roots {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                #[cfg(target_os = "windows")]
                let exe = entry.path().join("Editor").join("Unity.exe");
                #[cfg(target_os = "macos")]
                let exe = entry.path().join("Unity.app/Contents/MacOS/Unity");
                #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
                let exe = entry.path().join("Editor").join("Unity");
                if exe.is_file() {
                    return Some(exe);
                }
            }
        }
    }
    None
}

/// Whether the workspace looks like a Python project: a pytest/setup config, or any
/// `.py` file at the root (bounded scan — we don't recurse).
fn is_python_project(workspace: &Path) -> bool {
    for marker in [
        "pytest.ini",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "tox.ini",
    ] {
        if workspace.join(marker).is_file() {
            return true;
        }
    }
    std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".py")
        })
}

/// Whether the workspace has a `.sln` or `.csproj` at its root.
fn has_dotnet_project(workspace: &Path) -> bool {
    std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy().to_ascii_lowercase();
            name.ends_with(".sln") || name.ends_with(".csproj")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honors_explicit_non_default_command() {
        let got = iterate_verify_command(&Some("make test".into()), Path::new("/nope-xyz"));
        assert_eq!(got.as_deref(), Some("make test"));
    }

    #[test]
    fn treats_pytest_default_as_unset() {
        // Pytest default + an unrecognized dir → falls back to the configured value (the default).
        let got = iterate_verify_command(&Some(PYTEST_DEFAULT.into()), Path::new("/nope-xyz-2"));
        assert_eq!(got.as_deref(), Some(PYTEST_DEFAULT));
    }

    #[test]
    fn rust_project_overrides_the_stale_pytest_default_with_cargo_check() {
        // The live bug (2026-07-21): a Rust project kept the default `python -m pytest -q`, so the
        // build's compiler-driven fix loop ran pytest, found no rust errors, and never fixed the
        // real compile errors. A Cargo.toml MUST override the pytest default with `cargo check`.
        let dir = std::env::temp_dir().join(format!("dc-verify-rust-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let got = iterate_verify_command(&Some(PYTEST_DEFAULT.into()), &dir);
        assert_eq!(
            got.as_deref(),
            Some("cargo check"),
            "Rust project → cargo check"
        );
        // An EXPLICIT non-default command is still honored (the user knows best).
        let got = iterate_verify_command(&Some("cargo test -p void_sim".into()), &dir);
        assert_eq!(got.as_deref(), Some("cargo test -p void_sim"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
