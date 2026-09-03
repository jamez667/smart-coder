//! `sc-win` — the native Windows vibe-coding desktop app (spec 12 / M9).
//!
//! A thin shell over the proven core (spec 01): you type *intent* and watch the agent
//! (and the swarm) work. The host-testable logic lives here in the library (config mapping,
//! the worker bridge, the decision seams); the iced rendering glue lives in the binary
//! (`main.rs` + `app.rs`) and stays thin.
//!
//! It also **edits files** (spec 21). The CODE pane has two views: the read-only *review*
//! surface ([`codeview`]) that carries diffs, line comments and gates, and an *edit* view over a
//! real text buffer. The rules governing what may be edited and when a save is safe are in
//! [`editbuf`] — pure, so the parts that can lose your work are tested without a GUI.

pub mod board;
pub mod bridge;
pub mod chat;
pub mod chat_session;
pub mod claudecode;
pub mod claudesessions;
pub mod codeview;
pub mod comments;
pub mod comply;
pub mod config;
pub mod diagnostics;
pub mod editbuf;
pub mod filetree;
pub mod gitdiff;
pub mod layout;
pub mod linecomment;
pub mod persist;
pub mod plan;
pub mod proc;
pub mod project;
pub mod session;
pub mod splits;
pub mod terminal;
pub mod topology;
pub mod view;
pub mod welcome;

pub use board::{BoardRow, SubtaskStatus, SwarmBoard};
pub use bridge::{ChannelConfirmer, ChannelGate, Pending};
pub use codeview::{file_touched_by, is_mutating_touch, CodeView};
pub use config::{ToolCalling, UiConfig};
pub use diagnostics::{CompileReport, Diagnostic, Severity};
pub use editbuf::{Classified, DiskStamp, Ending, NoEdit, SaveVerdict};
pub use filetree::{build_rows, TreeRow};
pub use layout::{Axis, Layout, LayoutStore, PanelKind, PanelSlot, Side};
pub use plan::{Plan, PlanStep};
pub use project::{CompileCommand, ProjectKind};
pub use session::{RunKind, Session, UiEvent};
pub use topology::{Coder, CoderState, Flow, Peer, Topology};
pub use view::{agent_rows, swarm_rows, Row};
