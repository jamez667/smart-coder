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
