//! The CLI surface: argv → a resolved [`Cli`], and the things a run builds from it.
//!
//! Split by concern:
//!
//! * [`types`] — [`Cli`], [`Command`], [`ToolCallingArg`] and the defaults.
//! * [`parse`] — argv → [`Cli`], including the run-tail flag peel.
//! * [`config`] — flags → backends, agent/swarm config, permission policy.
//! * [`paths`] — session logs, replay resolution, test-file detection.
//! * [`doctor`] — the `doctor` report and the reachability probes.
//! * [`usage`] — the help text.

mod config;
mod doctor;
mod parse;
mod paths;
mod types;
mod usage;

#[cfg(test)]
mod tests;

pub use doctor::{doctor_report, preflight, probe};
pub use paths::{detect_test_files, resolve_replay_path, session_log_path, sessions_dir};
pub use types::{Cli, Command, ToolCallingArg, DEFAULT_BASE_URL, DEFAULT_MODEL};
pub use usage::usage;
