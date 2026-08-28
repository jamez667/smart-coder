//! The per-instance container: start it, patch it, run tests in it, tear it down.
//!
//! `sc_verify::Sandbox::Docker` is not reused here. It hardcodes
//! `-v <ws>:/workspace -w /workspace` and a bare `sh -c`, and a SWE-bench image needs
//! its own `/testbed` plus a conda activation — the mount would bury the very repo the
//! tests import. What *is* reused is the convention: the argv builders are pure
//! functions returning a `Command`, so they can be asserted without Docker installed.

use std::path::Path;
use std::process::Command;

use sc_proto::{DcError, Result};

/// Where every official SWE-bench image keeps the repository.
pub const TESTBED: &str = "/testbed";

/// Activate the image's conda env, then run `cmd` in the repo.
///
/// `sh` in these images is dash, which has no `source` — use `.` and `bash -c`.
/// `timeout` bounds it *inside* the container: killing the host process would leave
/// the container up and the pytest orphaned.
pub fn in_testbed(cmd: &str, timeout_secs: u64) -> String {
    format!(
        ". /opt/miniconda3/etc/profile.d/conda.sh && conda activate testbed && \
         cd {TESTBED} && timeout {timeout_secs} {cmd}"
    )
}

/// A running container for one instance, removed on drop.
#[derive(Debug)]
pub struct InstanceContainer {
    name: String,
}

impl InstanceContainer {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `docker run -d --name <n> <image> sleep infinity`.
    ///
    /// Long-lived rather than one `docker run --rm` per command: an instance is
    /// verified many times over an agent run, and re-entering a 2.8GB image each time
    /// would dominate the measurement.
    /// Where the host workspace is mounted inside the container.
    ///
    /// The agent edits on the host but its tests run in here, so without a shared view
    /// its verification would report on the container's *unedited* copy — the same
    /// result every turn, no matter what it changed. That is indistinguishable from a
    /// model that cannot make progress, and it is why this mount exists.
    pub const HOST_MOUNT: &'static str = "/hostws";

    pub fn start_args(name: &str, image: &str, host_ws: &Path) -> Vec<String> {
        vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.into(),
            "-v".into(),
            format!("{}:{}", host_ws.display(), Self::HOST_MOUNT),
            "--entrypoint".into(),
            "bash".into(),
            image.into(),
            "-c".into(),
            // `/workspace` -> `/testbed` so `sc_verify::Sandbox::Session` works
            // unmodified: it execs with `-w /workspace`, which these images do not
            // have, and that is a hard failure ("chdir to cwd failed"), not a
            // fallback. A symlink is cheaper and less invasive than teaching
            // sc-verify a second working directory.
            format!("ln -sfn {TESTBED} /workspace && sleep infinity"),
        ]
    }

    /// The name `sc_verify::SessionContainer` derives for `workspace`.
    ///
    /// Matching it is what lets the agent's `run_verification` reach *this* container:
    /// `SessionContainer::new` hashes the workspace path to a name and there is no
    /// constructor that takes one, so the container must be named to meet it.
    pub fn session_name_for(workspace: &Path) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        workspace.hash(&mut h);
        format!("sc-ws-{:016x}", h.finish())
    }

    pub fn exec_args(name: &str, script: &str) -> Vec<String> {
        vec![
            "exec".into(),
            name.into(),
            "bash".into(),
            "-c".into(),
            script.into(),
        ]
    }

    pub fn start(instance_id: &str, image: &str, host_ws: &Path) -> Result<InstanceContainer> {
        // Namespaced by pid so two runs on one machine cannot collide on the name.
        let name = format!(
            "sc-swe-{}-{}",
            instance_id.replace(['_', '/'], "-"),
            std::process::id()
        );
        Self::start_named(&name, image, host_ws)
    }

    /// Start under an exact name — used to meet [`Self::session_name_for`].
    pub fn start_named(name: &str, image: &str, host_ws: &Path) -> Result<InstanceContainer> {
        let name = name.to_string();
        // A leftover from a killed run would make `docker run --name` fail.
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        let out = Command::new("docker")
            .args(Self::start_args(&name, image, host_ws))
            .output()
            .map_err(|e| DcError::Eval(format!("docker run: {e}")))?;
        if !out.status.success() {
            return Err(DcError::Eval(format!(
                "starting {image}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(InstanceContainer { name })
    }

    /// Run a script inside the container. Returns (success, combined output).
    pub fn exec(&self, script: &str) -> Result<(bool, String)> {
        let out = Command::new("docker")
            .args(Self::exec_args(&self.name, script))
            .output()
            .map_err(|e| DcError::Eval(format!("docker exec: {e}")))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), text))
    }

    /// Copy a host file into the container.
    pub fn copy_in(&self, host: &Path, dest: &str) -> Result<()> {
        let out = Command::new("docker")
            .arg("cp")
            .arg(host)
            .arg(format!("{}:{dest}", self.name))
            .output()
            .map_err(|e| DcError::Eval(format!("docker cp in: {e}")))?;
        if !out.status.success() {
            return Err(DcError::Eval(format!(
                "copying {} in: {}",
                host.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Copy a path out of the container onto the host.
    ///
    /// Only ever the source subtree, never the whole repo: pylint's
    /// `tests/functional/s/symlink/` holds symlinks that make `docker cp` fail on
    /// Windows ("a required privilege is not held by the client"), and leaving the
    /// test tree in the container is what makes the frozen-test invariant structural
    /// rather than merely policed.
    pub fn copy_out(&self, src: &str, host: &Path) -> Result<()> {
        let out = Command::new("docker")
            .arg("cp")
            .arg(format!("{}:{src}", self.name))
            .arg(host)
            .output()
            .map_err(|e| DcError::Eval(format!("docker cp out: {e}")))?;
        if !out.status.success() {
            return Err(DcError::Eval(format!(
                "copying {src} out: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

impl Drop for InstanceContainer {
    /// Best-effort removal. A leaked 2.8GB container per instance would fill the disk
    /// within one run, so this runs on every exit path including a panic.
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_activates_conda_and_bounds_itself() {
        let s = in_testbed("python -m pytest", 300);
        assert!(s.contains("conda activate testbed"));
        assert!(s.contains("cd /testbed"));
        assert!(s.contains("timeout 300 python -m pytest"));
        // dash has no `source`.
        assert!(!s.contains("source "), "{s}");
    }

    #[test]
    fn the_container_runs_detached_under_bash_not_sh() {
        let a = InstanceContainer::start_args("n", "img:latest", Path::new("/tmp/ws"));
        assert!(a.contains(&"-d".to_string()));
        assert_eq!(a.iter().filter(|x| *x == "bash").count(), 1);
        assert!(a.contains(&"img:latest".to_string()));
    }

    #[test]
    fn exec_runs_through_bash() {
        let a = InstanceContainer::exec_args("n", "echo hi");
        assert_eq!(a[0], "exec");
        assert!(a.contains(&"bash".to_string()));
        assert!(a.contains(&"echo hi".to_string()));
    }
}
