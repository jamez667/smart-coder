//! Shared types and errors for `smart-coder`.
//!
//! Kept as small as it can be, so every other crate can lean on it. See the
//! design specs in `docs/specs/` (notably 01-architecture) for where this fits.
//!
//! ## Why the intake protocol lives here
//!
//! [`wire`] and [`intake`] describe the conversation between the developer's
//! daemon and the hosted intake server (spec 18), and `sc-daemon` re-exports
//! them so nothing in the workspace changed at its call sites.
//!
//! They sit here rather than in `sc-daemon` for one reason worth stating: the
//! public server would otherwise depend on `sc-daemon`, and through it on
//! `sc-model` and the whole local model stack, to obtain two type definitions.
//! Moving them makes "no model is anywhere near the public server" literally
//! true rather than true in spirit, and keeps its image build small.
//!
//! **That dependency line is the separation that matters** — not any repository
//! boundary. `sc-server` and the desktop agent share a workspace and are still
//! strangers in the build graph, because `cargo build -p sc-server` compiles
//! this crate and no other.
//!
//! **One definition, both ends.** Restating the protocol on the other side is
//! exactly the drift spec 17 exists to catch, so it is prevented by construction
//! instead of detected later.

pub mod intake;
pub mod wire;

pub use intake::IntakeKind;

use std::fmt;

/// The one error type that crosses crate boundaries.
///
/// Model misbehavior and eval failures are *normal, handled* conditions in
/// `smart-coder` (see spec 03), so they're plain variants here, never panics.
#[derive(Debug)]
pub enum DcError {
    /// A model backend failed or is unavailable.
    Backend(String),
    /// Something went wrong setting up or scoring an eval.
    Eval(String),
    /// A compliance pack was malformed, or an evidence run could not proceed.
    ///
    /// Distinct from `Eval` because this surfaces to an *auditor*: reporting a
    /// malformed SOC 2 pack as "eval error" would be actively misleading.
    Comply(String),
    /// An underlying I/O failure.
    Io(std::io::Error),
}

impl fmt::Display for DcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DcError::Backend(m) => write!(f, "backend error: {m}"),
            DcError::Eval(m) => write!(f, "eval error: {m}"),
            DcError::Comply(m) => write!(f, "compliance error: {m}"),
            DcError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for DcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DcError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DcError {
    fn from(e: std::io::Error) -> Self {
        DcError::Io(e)
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, DcError>;
