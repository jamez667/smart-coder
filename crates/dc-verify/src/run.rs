//! Running commands and verification (spec 04 — `run_command` /
//! `run_verification`).
//!
//! `run_command` returns the raw exit code + (truncated) output; the agent loop
//! truncates further for the window. `run_verification` runs the project's
//! configured test command and returns a structured [`TestReport`] by parsing the
//! output — the spine of the TDD loop (spec 11).

use std::path::Path;
use std::process::Command;

use crate::parse::parse;
use crate::report::TestReport;

/// Where a command runs: on the host, or inside a per-run ephemeral Docker container.
///
/// Docker mode is the reproducible build sandbox (spec 12): the workspace is mounted
/// into a fresh `--rm` container built from a pinned Python image, so generated code +
/// its tests run against a known toolkit and a known layout — never polluting or
/// depending on the host. The model backends stay on the host; only the *generated
/// app* runs here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Sandbox {
    /// Run directly on the host (the historical default; cmd/sh).
    #[default]
    Host,
    /// Run inside `docker run --rm` against `image`, with the workspace mounted at
    /// `/workspace`.
    Docker { image: String },
}

/// The captured result of running a shell command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub ok: bool,
    pub code: Option<i32>,
    /// Combined stdout+stderr.
    pub output: String,
}

/// Build the OS command that runs `command` in `workspace` under `sandbox`. Pure (no
/// I/O) so the Docker argument construction is host-testable without Docker present.
fn build_command(sandbox: &Sandbox, workspace: &Path, command: &str) -> Command {
    match sandbox {
        Sandbox::Host => {
            let (shell, flag) = if cfg!(windows) {
                ("cmd", "/C")
            } else {
                ("sh", "-c")
            };
            let mut c = Command::new(shell);
            c.arg(flag).arg(command).current_dir(workspace);
            c
        }
        Sandbox::Docker { image } => {
            // Per-run ephemeral container: `docker run --rm -v <ws>:/workspace
            // -w /workspace <image> sh -c "<command>"`. Docker handles the host→
            // container path translation for `-v` (including Windows paths), so we pass
            // the workspace verbatim. `sh -c` runs the verify command as one string.
            let mount = format!("{}:/workspace", workspace.display());
            let mut c = Command::new("docker");
            c.arg("run")
                .arg("--rm")
                .arg("-v")
                .arg(mount)
                .arg("-w")
                .arg("/workspace")
                .arg(image)
                .arg("sh")
                .arg("-c")
                .arg(command);
            c
        }
    }
}

/// Run `command` via the system shell inside `workspace` (host). Captures combined
/// stdout/stderr. A spawn failure is returned as a non-ok result with the error
/// as output, never a panic. The historical host-only entry point.
pub fn run_command(workspace: &Path, command: &str) -> CommandResult {
    run_command_in(&Sandbox::Host, workspace, command)
}

/// Run `command` in `workspace` under `sandbox`. Combined stdout/stderr captured; a
/// spawn failure (e.g. Docker not installed) is a non-ok result, never a panic.
pub fn run_command_in(sandbox: &Sandbox, workspace: &Path, command: &str) -> CommandResult {
    match build_command(sandbox, workspace, command).output() {
        Ok(out) => {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&err);
            }
            CommandResult {
                ok: out.status.success(),
                code: out.status.code(),
                output,
            }
        }
        Err(e) => CommandResult {
            ok: false,
            code: None,
            output: format!("failed to spawn {command:?}: {e}"),
        },
    }
}

/// Run the verification `command` on the host and parse it into a [`TestReport`].
pub fn run_verification(workspace: &Path, command: &str) -> TestReport {
    run_verification_in(&Sandbox::Host, workspace, command)
}

/// Run the verification `command` under `sandbox` and parse it into a [`TestReport`]
/// — the TDD gate, runnable inside the Docker build sandbox.
pub fn run_verification_in(sandbox: &Sandbox, workspace: &Path, command: &str) -> TestReport {
    let result = run_command_in(sandbox, workspace, command);
    parse(command, &result.output, result.ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-verify-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn docker_sandbox_builds_an_ephemeral_run_command() {
        // The Docker arg construction is pure, so we can assert it without Docker.
        let ws = std::path::Path::new("/tmp/ws");
        let sandbox = Sandbox::Docker {
            image: "dumb-coder-pyenv".to_string(),
        };
        let cmd = build_command(&sandbox, ws, "pytest -q");
        let prog = cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(prog, "docker");
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--rm".to_string()), "per-run ephemeral");
        assert!(
            args.contains(&"/tmp/ws:/workspace".to_string()),
            "workspace mounted: {args:?}"
        );
        assert!(args.contains(&"-w".to_string()) && args.contains(&"/workspace".to_string()));
        assert!(args.contains(&"dumb-coder-pyenv".to_string()), "the image");
        assert_eq!(args.last().unwrap(), "pytest -q", "the verify command");
    }

    #[test]
    fn host_sandbox_uses_the_system_shell() {
        let ws = std::path::Path::new(".");
        let cmd = build_command(&Sandbox::Host, ws, "echo hi");
        let prog = cmd.get_program().to_string_lossy().into_owned();
        assert!(prog == "cmd" || prog == "sh", "host uses the shell: {prog}");
    }

    #[test]
    fn run_command_captures_exit_and_output() {
        let ws = temp_dir("cmd");
        // `exit 3` is portable across sh and cmd.
        let r = run_command(&ws, "exit 3");
        assert!(!r.ok);
        assert_eq!(r.code, Some(3));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn run_command_reports_success() {
        let ws = temp_dir("cmd-ok");
        let r = run_command(&ws, "exit 0");
        assert!(r.ok);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn run_verification_generic_pass_fail() {
        let ws = temp_dir("verify");
        // A non-framework command -> generic report keyed off exit code.
        let red = run_verification(&ws, "exit 1");
        assert!(red.generic);
        assert!(!red.all_green());

        let green = run_verification(&ws, "exit 0");
        assert!(green.all_green());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
