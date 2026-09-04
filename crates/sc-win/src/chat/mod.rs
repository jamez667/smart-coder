//! [`Conversation`] — the plan-first chat engine. A multi-turn planning conversation with
//! the model, built on `sc_model`'s one primitive: a growing `Vec<Message>` sent to
//! `backend.generate`. The agent's job here is to *plan* (build up README.md / TODO.md as
//! real files), not to write source code — the system prompt enforces that.
//!
//! Pure/host-testable: no backend call and no iced types live here. The worker
//! ([`crate::chat_session`]) owns the actual `generate` call; this module owns *what to
//! send* (history + a mode-shaped system prompt) and *how to read the reply* (extracting the
//! `file:<name>` plan-file blocks the model proposes).
//!
//! Split by concern:
//!
//! * [`intent`] — the [`ChatIntent`] taxonomy, its grammar, and its per-intent instruction.
//! * [`conversation`] — [`Conversation`]: transcript, context injection, request building.
//! * [`reply`] — reading a raw reply back: prose, file blocks, command blocks, `<think>`.
//! * [`spec`] — spec paths, slugs, and wrapping a bare-prose plan into a file.

mod conversation;
mod intent;
mod reply;
mod spec;

#[cfg(test)]
mod tests;

pub use conversation::{Conversation, Mode};
pub use intent::ChatIntent;
pub use reply::{extract_command, parse_reply, proposed_fix, suggested_command, visible_so_far};
pub use spec::{is_spec_path, prepend_request, slug_for, spec_path, wrap_plan_prose};

/// A plan-file the assistant proposed in a reply (a ```file:NAME block). The app shows it in
/// the code view and writes it on the user's Apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedFile {
    /// The target filename, workspace-relative (e.g. `TODO.md`).
    pub name: String,
    /// The full proposed contents.
    pub content: String,
    /// Whether this file has been written to disk. A feature-plan card STAYS in the chat after
    /// applying (so its Breakdown/Build actions remain available) — this flips its Apply button to
    /// an "applied" state rather than removing the card. Non-plan files are removed on apply as
    /// before, so this stays false for them.
    pub applied: bool,
}

/// One turn shown in the chat thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Speaker,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    You,
    Agent,
    /// A debug echo (the raw prompt sent to the model), shown only in debug mode.
    Debug,
}
