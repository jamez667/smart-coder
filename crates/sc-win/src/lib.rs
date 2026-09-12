//! `sc-win` — **Smart Coder**, the agent product (spec 12 / M9).
//!
//! # Smart Coder is the Crafter plus an agent
//!
//! The editor half — buffers and save rules, the panel tree, git, the file tree, the
//! terminal, compile diagnostics, the profiler — lives in [`sc_craft_ui`] and is shared
//! verbatim with `smart-coder-crafter`, which is the same editor with nothing bolted on.
//! This crate is the "plus an agent" part: chat, the swarm, review gates, the remote
//! mirror, the model backends, and the config that points at them.
//!
//! That was a runtime setting until the two products split (spec 21). Craft mode was a
//! predicate (`cfg.craft()`) consulted at every backend builder, backed by a Cargo feature
//! that pinned it true and ~1,600 lines of tests proving each refusal fired. All of it is
//! gone, replaced by a dependency edge that only points one way: the Crafter cannot
//! contact a model because no crate that can reach one is in its tree, which
//! `scripts/check.*` asserts with `cargo tree`.
//!
//! The editor modules are re-exported below under their old paths, so `sc_win::layout`,
//! `sc_win::gitdiff` and the rest still resolve — the call sites did not have to move when
//! the code did.
//!
//! # What is genuinely this crate's
//!
//! The host-testable logic lives here in the library (config mapping, the worker bridge,
//! the decision seams); the iced rendering glue lives in the binary (`main.rs` + `app/`)
//! and stays thin.
//!
//! Two modules here are the agent halves of files that split at the crate boundary:
//! [`follow`] ("follow the agent" — which file the CODE pane shows as tools touch files)
//! came out of [`sc_craft_ui::codeview`], and [`sendback`] (line comments becoming
//! workflow feedback) came out of [`sc_craft_ui::comments`]. Both read types the Crafter
//! has no access to — `sc_core::AgentEvent` and `sc_workflow::Phase` — which is exactly
//! why they could not travel with the rest.

pub mod board;
pub mod bridge;
pub mod chat;
pub mod chat_session;
pub mod claudecode;
pub mod claudesessions;
pub mod comply;
pub mod config;
pub mod follow;
pub mod linecomment;
pub mod plan;
pub mod sendback;
pub mod session;
pub mod topology;
pub mod view;

// --- The editor half, re-exported under its historical paths ---
//
// These were modules of this crate until the split. Re-exporting rather than rewriting
// every `sc_win::layout::…` call site keeps that move invisible to the agent binary, which
// is a large amount of churn avoided for one line each.
pub use sc_craft_ui::{
    codeview, comments, diagnostics, editbuf, filetree, flame, flamecanvas, gitdiff, layout,
    markdown, minimap, persist, proc, project, splits, terminal, welcome,
};

pub use board::{BoardRow, SubtaskStatus, SwarmBoard};
pub use bridge::{ChannelConfirmer, ChannelGate, Pending};
pub use config::{ToolCalling, UiConfig};
pub use follow::{file_touched_by, is_mutating_touch};
pub use plan::{Plan, PlanStep};
pub use session::{RunKind, Session, UiEvent};
pub use topology::{Coder, CoderState, Flow, Peer, Topology};
pub use view::{agent_rows, swarm_rows, Row};

// Editor types that were re-exported from this crate's root before the split, kept so the
// binary's `use sc_win::{CodeView, Layout, …}` lines still resolve.
pub use sc_craft_ui::{
    build_rows, flame_layout, parse_folded, Axis, Classified, CodeView, CompileCommand,
    CompileReport, Diagnostic, DiskStamp, Ending, Frame, Layout, LayoutStore, NoEdit, PanelKind,
    PanelSlot, Placed, Product, Profile, ProjectKind, SaveVerdict, Severity, Side, TreeRow,
};
