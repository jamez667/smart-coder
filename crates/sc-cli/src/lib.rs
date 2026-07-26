//! `smart-coder` CLI — the M0 surface (spec 06): a `doctor` check and a trivial
//! chat loop against a *real* backend.
//!
//! The interesting, testable logic lives here (arg parsing, the doctor report,
//! backend construction); [`crate::main`] is a thin I/O shell over it. This keeps
//! the binary unit-tested in the project's TDD style.
//!
//! M0 scope is deliberately small: prompt → model text → print, **no tools**. The
//! tool-driven agent loop already lives in `sc-core`; wiring it behind a `run`
//! subcommand is M1+ work.

mod cli;

pub use cli::{
    detect_test_files, doctor_report, preflight, probe, resolve_replay_path, session_log_path,
    sessions_dir, usage, Cli, Command, ToolCallingArg, DEFAULT_BASE_URL, DEFAULT_MODEL,
};
