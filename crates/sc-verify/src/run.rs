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

/// A **long-lived** per-workspace sandbox container: started once, then commands
/// `docker exec` into it so working-directory changes, environment, and installed
/// dependencies persist across commands — a real interactive shell, not a fresh
/// container per command (the [`Sandbox::Docker`] verify model).
///
/// It backs the IDE's integrated terminal (and, later, the agent's own command
/// execution): every command runs on the container's Linux userland, never the
/// Windows host, and — being Linux — it cannot even produce a runnable host `.exe`.
///
/// Every method here is **pure** (returns a [`Command`] to spawn, does no I/O), so the
/// docker argument construction is host-testable without Docker installed, matching the
/// rest of this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContainer {
    /// The `docker --name` for this workspace's container (stable across restarts of
    /// the app for the same workspace path, so a leftover container is reused/cleaned).
    name: String,
    /// The image to run.
    image: String,
}

impl SessionContainer {
    /// A session container for `workspace` using `image`. The container name is derived
    /// from the workspace path so it's stable (same project → same name) and unique
    /// (different projects don't collide), keeping leftover-container cleanup simple.
    pub fn new(workspace: &Path, image: impl Into<String>) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        workspace.hash(&mut h);
        Self {
            name: format!("sc-ws-{:016x}", h.finish()),
            image: image.into(),
        }
    }

    /// The container's docker name (also the handle used by [`Self::stop_command`]).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build the command that **starts** the detached, long-lived container: it mounts
    /// `workspace` at `/workspace` and idles on `sleep infinity` so it stays alive for
    /// `exec`s until explicitly removed. `--rm` so it self-cleans when force-removed or
    /// the daemon restarts. (Network/cap hardening is a deliberate follow-up — builds
    /// currently need network to fetch dependencies.)
    pub fn start_command(&self, workspace: &Path) -> Command {
        let mount = format!("{}:/workspace", workspace.display());
        let mut c = Command::new("docker");
        c.arg("run")
            .arg("-d")
            .arg("--rm")
            .arg("--name")
            .arg(&self.name)
            .arg("-v")
            .arg(mount)
            .arg("-w")
            .arg("/workspace")
            .arg("--entrypoint")
            .arg("sh")
            .arg(&self.image)
            .arg("-c")
            .arg("sleep infinity");
        c
    }

    /// Build the command that runs `command` **inside** the running container via
    /// `docker exec`, from `/workspace`. `sh -c "<command>"` so shell built-ins, pipes,
    /// and operators work — a real terminal line. This is the streaming entry point the
    /// terminal spawns with piped stdio.
    pub fn exec_command(&self, command: &str) -> Command {
        let mut c = Command::new("docker");
        c.arg("exec")
            .arg("-w")
            .arg("/workspace")
            .arg(&self.name)
            .arg("sh")
            .arg("-c")
            .arg(command);
        c
    }

    /// Build the command that **force-removes** the container (stop + delete). Used on
    /// project switch / app close, and to clear a stale container before a fresh start.
    pub fn stop_command(&self) -> Command {
        let mut c = Command::new("docker");
        c.arg("rm").arg("-f").arg(&self.name);
        c
    }
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
            "sc-verify-{tag}-{}-{}",
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
            image: "smart-coder-pyenv".to_string(),
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
        assert!(args.contains(&"smart-coder-pyenv".to_string()), "the image");
        assert_eq!(args.last().unwrap(), "pytest -q", "the verify command");
    }

    #[test]
    fn host_sandbox_uses_the_system_shell() {
        let ws = std::path::Path::new(".");
        let cmd = build_command(&Sandbox::Host, ws, "echo hi");
        let prog = cmd.get_program().to_string_lossy().into_owned();
        assert!(prog == "cmd" || prog == "sh", "host uses the shell: {prog}");
    }

    /// Collect a `Command`'s program + args as owned strings, for pure assertions.
    fn parts(cmd: &Command) -> (String, Vec<String>) {
        (
            cmd.get_program().to_string_lossy().into_owned(),
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn session_name_is_stable_per_workspace_and_distinct_across() {
        let a1 = SessionContainer::new(Path::new("/tmp/proj-a"), "img");
        let a2 = SessionContainer::new(Path::new("/tmp/proj-a"), "img");
        let b = SessionContainer::new(Path::new("/tmp/proj-b"), "img");
        assert_eq!(a1.name(), a2.name(), "same workspace → same container name");
        assert_ne!(a1.name(), b.name(), "different workspace → different name");
        assert!(a1.name().starts_with("sc-ws-"), "name: {}", a1.name());
    }

    #[test]
    fn session_start_is_a_detached_long_lived_mount() {
        let sc = SessionContainer::new(Path::new("/tmp/ws"), "smart-coder-pyenv");
        let (prog, args) = parts(&sc.start_command(Path::new("/tmp/ws")));
        assert_eq!(prog, "docker");
        assert_eq!(args[0], "run");
        assert!(args.contains(&"-d".to_string()), "detached: {args:?}");
        assert!(args.contains(&"--name".to_string()));
        assert!(args.contains(&sc.name().to_string()), "named: {args:?}");
        assert!(
            args.contains(&"/tmp/ws:/workspace".to_string()),
            "workspace mounted: {args:?}"
        );
        assert!(args.contains(&"smart-coder-pyenv".to_string()), "the image");
        // Idles alive so exec's can attach.
        assert!(args.contains(&"sleep infinity".to_string()), "kept alive: {args:?}");
    }

    #[test]
    fn session_exec_targets_the_container_via_shell() {
        let sc = SessionContainer::new(Path::new("/tmp/ws"), "img");
        let (prog, args) = parts(&sc.exec_command("ls -la | wc -l"));
        assert_eq!(prog, "docker");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&sc.name().to_string()), "into our container");
        assert!(args.contains(&"-w".to_string()) && args.contains(&"/workspace".to_string()));
        // The whole line is one shell argument, so pipes/operators survive.
        assert_eq!(args.last().unwrap(), "ls -la | wc -l");
        let sh = args.iter().position(|a| a == "sh").unwrap();
        assert_eq!(args[sh + 1], "-c", "runs via sh -c");
    }

    #[test]
    fn session_stop_force_removes_by_name() {
        let sc = SessionContainer::new(Path::new("/tmp/ws"), "img");
        let (prog, args) = parts(&sc.stop_command());
        assert_eq!(prog, "docker");
        assert_eq!(args, vec!["rm", "-f", sc.name()]);
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
