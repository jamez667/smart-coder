//! The terminal pump — all that is left of what was the agent's event loop.
//!
//! `pump()`, `pump_chat()`, `pump_remote()`, the health probe, the confirm/gate answers
//! and the sandbox plumbing all left for `sc-plugin-agent` (spec 25).
//!
//! The container exec modes went with them, and that is a real loss worth naming: the
//! integrated terminal could run inside the workspace's Docker container, and
//! `sc_verify::Sandbox` was how it knew which. `sc-verify` is an agent crate, so the
//! terminal is host-only until that type is re-homed to a leaf.

use super::*;

impl App {
    /// Drain the terminal's output channel into its scrollback.
    ///
    /// Called on every tick while a command is running. The channel is drained rather
    /// than read one line per tick: a build writes faster than 50ms, and taking one line
    /// at a time would fall steadily further behind the process producing them.
    pub(crate) fn pump_terminal(&mut self) {
        let Some(rx) = &self.term_rx else {
            return;
        };
        // `drain` takes every line waiting on the channel and reports whether the child
        // exited. Draining rather than reading one line per tick matters: a build writes
        // faster than the 50ms tick, so one-at-a-time would fall steadily further behind.
        if self.terminal.drain(rx) {
            self.term_rx = None;
        }
    }

    /// The exec mode for a command **the user clicked**, which is always the host.
    ///
    /// The agent's own runs were contained; a command the user chose is a different act.
    /// They clicked a button labelled "Run in terminal", in their own project, on their
    /// own machine.
    ///
    /// Observed before this was split out: clicking Run on `cargo run -p void_claim
    /// --release` produced a terminal and nothing else. `use_docker` defaulted to true and
    /// the default image was `smart-coder-pyenv` — a Python image with no cargo — so the
    /// command could not have built. And even in a Rust image it could not have RUN:
    /// void_claim opens a window, and a container on Windows has no display to open it on.
    /// The button inherited the agent's containment and could never do what it said.
    pub(crate) fn user_exec_mode(&self) -> sc_win::terminal::ExecMode {
        let cwd = self
            .picked_workspace
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        sc_win::terminal::ExecMode::Host { cwd }
    }
}
