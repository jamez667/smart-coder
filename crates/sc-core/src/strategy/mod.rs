//! Tool-call strategies — the heart of the M1 reliability story (spec 02/04).
//!
//! Getting a *well-formed* tool call out of a small model is the hardest
//! small-model problem. The harness adapts its approach to what the backend can
//! enforce (spec 02 — capabilities):
//!
//! | Backend supports          | Strategy                                   |
//! | ------------------------- | ------------------------------------------ |
//! | GBNF grammar (llama.cpp)  | constrain decoding to the tool grammar     |
//! | JSON-schema / native FC   | native tool-calling / schema mode          |
//! | nothing (plain completion)| prompt + parse + **repair loop**           |
//!
//! Every strategy shares one post-condition: turn a model turn into either a
//! validated [`ValidatedCall`] or a structured [`RepairError`] that the loop
//! renders into a precise re-prompt. The strategy owns *how* tools are presented
//! and decoded; the loop owns budgets, observation feedback, and stopping.
//!
//! Split by concern:
//!
//! * [`error`] — [`RepairError`] and the [`ToolCallStrategy`] contract.
//! * [`kinds`] — the three strategies and [`select_strategy`].
//! * [`repair`] — salvaging a real call out of a broken reply.
//!
//! [`ValidatedCall`]: sc_tools::ValidatedCall

mod error;
mod kinds;
mod repair;

#[cfg(test)]
mod tests;

pub use error::{RepairError, ToolCallStrategy};
pub use kinds::{select_strategy, Grammar, NativeTools, ParseRepair};
pub use repair::{extract_markdown_write, extract_write_batch, is_truncated_write_salvage};
