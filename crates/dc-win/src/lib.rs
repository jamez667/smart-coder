//! `dc-win` — the native Windows vibe-coding desktop app (spec 12 / M9).
//!
//! A thin shell over the proven core (spec 01): you type *intent* and watch the agent
//! (and the swarm) work — no code editor. The host-testable logic lives here in the
//! library (config mapping, the worker bridge, the decision seams); the iced
//! rendering glue lives in the binary (`main.rs` + `app.rs`) and stays thin.

pub mod board;
pub mod bridge;
pub mod config;
pub mod plan;
pub mod session;
pub mod topology;
pub mod view;

pub use board::{BoardRow, SubtaskStatus, SwarmBoard};
pub use bridge::{ChannelConfirmer, ChannelGate, Pending};
pub use config::{ToolCalling, UiConfig};
pub use plan::{Plan, PlanStep};
pub use session::{RunKind, Session, UiEvent};
pub use topology::{Coder, CoderState, Flow, Peer, Topology};
pub use view::{agent_rows, swarm_rows, Row};
