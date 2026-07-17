//! Shared types and errors for `smart-coder`.
//!
//! Kept dependency-free so every other crate can lean on it. See the design
//! specs in `docs/specs/` (notably 01-architecture) for where this fits.

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
    /// An underlying I/O failure.
    Io(std::io::Error),
}

impl fmt::Display for DcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DcError::Backend(m) => write!(f, "backend error: {m}"),
            DcError::Eval(m) => write!(f, "eval error: {m}"),
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
