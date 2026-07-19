//! Windowless child-process spawning (mirrors `sc-win`'s `proc`): on Windows a GUI
//! or long-lived process shelling out to `git` would pop a transient console window
//! per call. `command` sets `CREATE_NO_WINDOW` on Windows and is a plain
//! `Command::new` everywhere else.

use std::ffi::OsStr;
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`Command`] for `program` that won't spawn a visible console window.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}
