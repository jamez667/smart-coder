//! Windowless subprocess spawning.
//!
//! A copy of `sc_craft_ui::proc`, and deliberately so: this crate is a plugin, and a
//! plugin that linked the editor to obtain a `Command` builder would defeat the point of
//! being a separate process. Twenty lines against a dependency on the whole UI crate.
//!
//! The behaviour is the one thing that matters here and it is easy to lose: on Windows a
//! plain `Command::new` pops a console window for every child. The agent shells out to
//! `git` constantly — status, diff, revert — so without `CREATE_NO_WINDOW` a run flashes
//! hundreds of black terminals across the screen. That was a real bug in the desktop app
//! before every spawn was routed through a helper like this one.

use std::ffi::OsStr;
use std::process::Command;

/// `CREATE_NO_WINDOW` — suppress the console window Windows allocates for a child.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`Command`] that spawns no console window on Windows.
///
/// **Every** subprocess this crate starts goes through here. A direct `Command::new`
/// anywhere in the agent is a bug that shows up as flashing terminals, and only on
/// Windows, and only when that code path runs.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    // `mut` is used only on the Windows branch; elsewhere the binding is returned
    // untouched, so it is intentionally-mutable there.
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
