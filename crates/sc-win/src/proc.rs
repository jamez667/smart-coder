//! Child-process spawning that never flashes a console window.
//!
//! On Windows, spawning a console subprocess (git, cargo, …) from a GUI app pops a
//! transient `conhost` window for each call. `sc-win` shells out to `git` many
//! times per refresh (see [`crate::gitdiff`]), which otherwise flickers hundreds of
//! black terminals. Every spawn in this crate goes through [`command`] /
//! [`git`], which set `CREATE_NO_WINDOW` on Windows and are a plain
//! `Command::new` everywhere else.

use std::ffi::OsStr;
use std::process::Command;

/// `CreationFlags` bit that suppresses the console window (winbase.h).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`Command`] for `program` that won't spawn a visible console window.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    // `mut` is used only on the Windows branch below; on other targets `c` is
    // returned untouched, so the binding is intentionally-mutable there.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Shorthand for the most common case: a windowless `git` invocation.
pub fn git() -> Command {
    command("git")
}

/// Kill a process **and everything it spawned**.
///
/// `Child::kill` terminates only the process you hold a handle to. That is the wrong tool for
/// anything that shells out: `samply record -- cargo run` is three processes deep, and killing
/// the middle one leaves the profiled binary running with nobody watching it. Windows has no
/// process-group signal, so the tree walk is `taskkill /T`; elsewhere the negative pid signals
/// the process group.
///
/// Best-effort and non-blocking: the child is already being waited on by its caller, and the
/// resulting stream close delivers the normal exit path.
pub fn kill_tree(pid: u32) {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = command("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = command("kill");
        // The NEGATIVE pid is the process group — the POSIX equivalent of `/T`.
        c.args(["-TERM", &format!("-{pid}")]);
        c
    };
    let _ = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Open a local file in the user's default application (the browser, for HTML).
///
/// Windows has no `xdg-open`; the shell verb lives in `explorer.exe`, which takes
/// the path directly and hands it to the registered handler. Deliberately NOT
/// `cmd /C start`, which would treat a path containing `&` as a command
/// separator — a real hazard for a path the user chose.
///
/// Best-effort: a failure to launch a viewer is not a reason to fail the work
/// that produced the file, so this reports the error and the caller carries on.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // `explorer.exe` returns a non-zero exit code even on success, so spawn
        // and detach rather than checking status.
        Command::new("explorer").arg(path).spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        command("xdg-open").arg(path).spawn()?;
        Ok(())
    }
}
