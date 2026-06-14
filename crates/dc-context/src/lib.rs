//! `dc-context` — the Context Manager (spec 05).
//!
//! For a small model, deciding *what goes into each prompt* is the difference
//! between a working agent and a confused one: a tiny window gets confused by
//! irrelevant context long before it runs out of tokens. This crate treats the
//! window as a scarce, hard-budgeted resource and assembles each turn's prompt to
//! fit — under prioritized zones, with observation truncation and rolling history
//! compaction.
//!
//! It is pure logic over `dc_model` message/token primitives; retrieval of the
//! *content* for the `Retrieved` zone lives in `dc-index`, and the agent loop in
//! `dc-core` wires the two together.

mod budget;
mod history;
mod tokens;
mod truncate;

pub use budget::{prompt_budget, BuiltContext, ContextBuilder, Role, Segment, Zone};
pub use history::{split_for_compaction, summarize_history, TurnRecord};
pub use tokens::{estimate_tokens, TokenCounter};
pub use truncate::truncate_observation;
