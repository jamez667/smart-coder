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

/// Put Python on PATH for this benchmark's images, then run `cmd` in the repo.
///
/// `prefix` comes from [`super::instance::Benchmark::python_prefix`] — conda activation
/// for SWE-bench, nothing for SWE-bench-Live, whose images carry a plain system Python.
/// `sh` in these images is dash, which has no `source`, so the prefix uses `.` and
/// everything runs under `bash -c`.
///
/// `timeout` bounds the command *inside* the container: killing the host process would
/// leave the container up and the pytest orphaned.
pub fn in_testbed(prefix: &str, cmd: &str, timeout_secs: u64) -> String {
    format!("{prefix}cd {TESTBED} && timeout {timeout_secs} {cmd}")
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

    /// Where the host workspace is mounted inside the container.
    ///
    /// The agent edits on the host but its tests run in here, so without a shared view
    /// its verification would report on the container's *unedited* copy — the same
    /// result every turn, no matter what it changed. That is indistinguishable from a
    /// model that cannot make progress, and it is why this mount exists.
    pub const HOST_MOUNT: &'static str = "/hostws";

    /// `docker run -d --name <n> <image> sleep infinity`.
    ///
    /// Long-lived rather than one `docker run --rm` per command: an instance is
    /// verified many times over an agent run, and re-entering a multi-GB image each
    /// time would dominate the measurement.
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

    /// Copy a directory tree from the host back into the container, via a tar stream.
    ///
    /// **Not `docker cp`**, which replaces whole paths: the host copy has real
    /// directories where the container has symlinks (see [`Self::copy_out`] on `-h`),
    /// so it fails with `cannot overwrite non-directory "/testbed/kubernetes/config"
    /// with directory`. Extracting a tar merges entry by entry and writes *through* the
    /// container's symlinks, leaving its structure intact and only updating file
    /// contents — which is all the agent changed.
    pub fn copy_dir_in(&self, host_dir: &Path, leaf: &str, dest_parent: &str) -> Result<()> {
        let tar = Command::new("tar")
            .arg("cf")
            .arg("-")
            .arg("-C")
            .arg(host_dir)
            .arg(leaf)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| DcError::Eval(format!("host tar: {e}")))?;

        let out = Command::new("docker")
            .arg("exec")
            .arg("-i")
            .arg(&self.name)
            .arg("bash")
            .arg("-c")
            .arg(format!("tar xf - -C '{dest_parent}'"))
            .stdin(
                tar.stdout
                    .ok_or_else(|| DcError::Eval("tar stdout".into()))?,
            )
            .output()
            .map_err(|e| DcError::Eval(format!("docker exec tar in: {e}")))?;
        if !out.status.success() {
            return Err(DcError::Eval(format!(
                "copying {} back: {}",
                host_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Copy a path out of the container onto the host, via a tar stream.
    ///
    /// `host` is where the copied entry LANDS, not a directory to drop it in:
    /// `copy_out("/testbed/pylint", ws.join("src/pylint"))`. Both callers must agree,
    /// and they did not — one passed the containing directory, which put every test
    /// file inside a directory named after itself.
    ///
    /// **Not `docker cp`.** On Windows that tries to recreate a symlink as a real
    /// filesystem symlink, which needs a privilege an ordinary user does not have:
    /// `kubernetes-client/python` keeps four symlinked directories inside `kubernetes/`
    /// and the whole instance failed with "a required privilege is not held by the
    /// client". Piping `tar` through and extracting on the host keeps symlinks as tar
    /// entries, which extract without elevation. Measured on that image: `docker cp`
    /// fails, the tar stream extracts 41MB cleanly.
    ///
    /// `tar -h` (dereference) is needed as well: a plain stream preserves the symlink
    /// as a symlink, and extracting *that* on Windows hits the same privilege wall —
    /// "Cannot create symlink to 'base/dynamic'". Dereferencing writes real
    /// directories instead. In this repo the links point inside the same subtree
    /// (`kubernetes/config` -> `base/config`), so it duplicates content already in the
    /// copy rather than pulling anything new in; measured, the tree extracts at 44MB.
    ///
    /// The cost is that an agent edit under a dereferenced path lands in the copy, not
    /// the link target, so the copy-back writes a real directory over the container's
    /// symlink. That is a change in shape the harness does not reconcile — acceptable
    /// only because the fix is scored by running the tests, not by diffing the tree.
    pub fn copy_out(&self, src: &str, host: &Path) -> Result<()> {
        let (parent, leaf) = src.rsplit_once('/').unwrap_or((".", src));
        let tar = Command::new("docker")
            .args(Self::exec_args(
                &self.name,
                &format!("cd '{parent}' && tar chf - '{leaf}'"),
            ))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| DcError::Eval(format!("docker exec tar: {e}")))?;

        // `host` names where the copied entry should LAND, so tar extracts into its
        // parent — tar already writes the leaf itself. Creating `host` as a directory
        // instead makes a directory named after the file, with the real file inside it:
        // `read_file tests/data/test_mm_plugin.py` then failed with "Access is denied"
        // (os error 5, which is what Windows returns for reading a directory), and the
        // agent could not read the very test it was asked to satisfy. Measured on
        // hiyouga__llama-factory-7505: three denied reads across one run.
        let extract_into = host.parent().unwrap_or(host);
        std::fs::create_dir_all(extract_into)
            .map_err(|e| DcError::Eval(format!("creating {}: {e}", extract_into.display())))?;
        let out = Command::new("tar")
            .arg("xf")
            .arg("-")
            .arg("-C")
            .arg(extract_into)
            .stdin(
                tar.stdout
                    .ok_or_else(|| DcError::Eval("tar stdout".into()))?,
            )
            .output()
            .map_err(|e| DcError::Eval(format!("host tar: {e}")))?;
        if !out.status.success() {
            return Err(DcError::Eval(format!(
                "extracting {src}: {}",
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
        let s = in_testbed(
            super::super::instance::Benchmark::SweBench.python_prefix(),
            "python -m pytest",
            300,
        );
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
