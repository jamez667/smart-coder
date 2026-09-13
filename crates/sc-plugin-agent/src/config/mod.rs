//! [`UiConfig`] — the GUI's config surface, a plain owned mirror of the `sc-cli`
//! `Cli` fields the settings panel edits (spec 06/12). It carries no borrows and no
//! iced types, so it is `Send` and host-testable: the worker thread builds the
//! backends and the `sc_core::AgentConfig` / `sc_swarm::SwarmConfig` *from an owned
//! clone*, exactly the way `Cli::backend()` / `agent_config()` / `swarm_config()` do
//! — the GUI is just another front-end producing the same config (spec 01).
//!
//! Split by concern:
//!
//! * [`types`] — [`UiConfig`], the [`Connection`]/[`Provider`] model, and the defaults.
//! * [`load`] — env → config.json → default precedence, migration, and write-back.
//! * [`build`] — a resolved config → backends, agent/swarm config, sandbox.
//! * [`file`] — where config.json lives and its pure parse/serialize pair.
//! * [`workspace`] — workspace defaults, the source-file ledger, verify detection.

mod build;
mod file;
mod load;
mod types;
mod workspace;

#[cfg(test)]
mod tests;

pub use file::{log_dir, ConfigFields, GEMINI_OPENAI_BASE_URL};
pub use types::{Connection, Provider, ToolCalling, UiConfig};
pub use workspace::{default_workspace, detect_verify_command, repo_overview, source_files};
