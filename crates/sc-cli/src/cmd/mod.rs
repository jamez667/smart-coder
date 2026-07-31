//! One module per subcommand family. Each exposes the `fn(&Cli, …) -> ExitCode`
//! that `main` dispatches to; the shared plumbing lives in [`common`].

pub mod chat;
pub mod common;
pub mod comply;
pub mod plan;
pub mod queue;
pub mod replay;
pub mod run;
pub mod swarm;
pub mod trace;
