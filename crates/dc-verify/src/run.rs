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

/// The captured result of running a shell command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub ok: bool,
    pub code: Option<i32>,
    /// Combined stdout+stderr.
    pub output: String,
}

/// Run `command` via the system shell inside `workspace`. Captures combined
/// stdout/stderr. A spawn failure is returned as a non-ok result with the error
/// as output, never a panic.
pub fn run_command(workspace: &Path, command: &str) -> CommandResult {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    match Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(workspace)
        .output()
    {
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

/// Run the verification `command` and parse its output into a [`TestReport`].
pub fn run_verification(workspace: &Path, command: &str) -> TestReport {
    let result = run_command(workspace, command);
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
