//! Report renderers. Three audiences, three formats.
//!
//! All are pure functions over an [`EvidencePack`](crate::evidence::EvidencePack)
//! with no I/O, which is what makes them assertable in tests — and why the
//! pack's timestamp is injected rather than sampled from a clock.

pub mod json;
pub mod markdown;
pub mod sarif;
