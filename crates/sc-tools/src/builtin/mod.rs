//! The built-in v1 tool surface and its execution (spec 04 — built-in tool set).
//!
//! Deliberately tiny and narrow: a few sharply-scoped tools beat a broad,
//! ambiguous surface for a small model (spec 04). The surface spans read-only
//! navigation (`read_file`/`list_dir`/`search_code`/`find_symbol`), mutation
//! (`write_file`/`create_file`/anchored `edit_file`), and execution
//! (`run_command`/`run_verification`) — the latter two run processes, so the
//! agent loop executes them; this module is the pure-filesystem half.
//!
//! Every path is sandboxed to the workspace root; traversal outside it is
//! rejected. Execution never panics — tool errors become structured observations
//! the model can react to.
//!
//! Split across submodules by concern:
//!
//! * [`registry`] — the tool schemas (what the model is offered).
//! * [`dispatch`] — [`ToolOutcome`] and [`execute`]: name → implementation.
//! * [`read`] — read-only navigation.
//! * [`write`] — the mutating tools.
//! * [`guards`] — the pre-write tripwires that catch small-model corruption.
//! * [`util`] — workspace helpers ([`safe_join`], [`source_files`]).

mod dispatch;
mod guards;
mod read;
mod registry;
mod util;
mod write;

#[cfg(test)]
mod tests;

pub use dispatch::{execute, handled_here, ToolOutcome, NOT_EXECUTED_HERE};
pub use registry::{default_registry, minimal_worker_registry};
pub use util::{safe_join, source_files};
