//! **The agent, as a plugin** (spec 25).
//!
//! Chat, the runs, the swarm, the review gates and the model configuration — everything
//! `sc-win` used to compile in. The editor keeps the editor.
//!
//! # Why this is a library as well as a binary
//!
//! The four pieces moved here — [`chat`], [`chat_session`], [`session`], [`bridge`] —
//! were written with **no iced types in them**, deliberately, years before there was a
//! plugin boundary to cross. `session/mod.rs` says so in its own header, and `bridge.rs`
//! says the whole confirm/gate protocol is host-testable because of it. That is why they
//! moved nearly untouched, and it is why they stay a library: their tests are worth more
//! than the binary wrapper around them.
//!
//! # What changed in the move
//!
//! Three host symbols had to be re-homed, and only three:
//!
//! * `proc::git` — a windowless `Command`. Copied into [`proc`] rather than depended on:
//!   a plugin that linked the editor to obtain a `Command` builder would defeat being a
//!   separate process.
//! * `config::repo_overview` — already a re-export of `sc_iterate::repo_overview`, and
//!   this crate depends on `sc-iterate`, so it resolves unchanged.
//! * `config::log_dir` — now points at this plugin's own directory. The host's transcript
//!   folder is the host's.
//!
//! Everything else the moved files referenced was internal cross-traffic between the four
//! of them.

pub mod board;
pub mod bridge;
pub mod chat;
pub mod chat_session;
pub mod config;
pub mod follow;
pub mod linecomment;
pub mod plan;
pub mod proc;
pub mod sendback;
pub mod session;
pub mod topology;
pub mod view;

pub use bridge::{ChannelConfirmer, ChannelGate, Pending};
pub use config::{ToolCalling, UiConfig};
pub use session::{RunKind, Session, UiEvent};
