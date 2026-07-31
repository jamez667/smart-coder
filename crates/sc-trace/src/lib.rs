//! `sc-trace` — spec traceability & drift (spec 17).
//!
//! A spec that cannot be checked will drift, and the drift will be silent. This
//! repo has the evidence: the pipeline ran on five phases while every spec
//! described six; `sc-cli` printed "6 phase artifacts" while writing five. None
//! of it was *wrong code*, so no test caught it — every instance was found by a
//! human reading two documents side by side.
//!
//! The commitment:
//!
//! > **Drift is detected by a machine, not by remembering to look.**
//!
//! This is the boring, deterministic first layer. No model runs. The
//! `spec-guardian` agent stays as the semantic second layer above it, reading
//! meaning that anchors cannot capture — this removes the load-bearing cases
//! from its shoulders so its judgment is spent where judgment is required.
//!
//! Two directions, needing different machinery:
//!
//! * **Stale claims** — a spec asserts something the code no longer does. Caught
//!   by [`anchor`]s: `<!--@ sc_workflow::Phase::ALL len=5 -->` fails the moment a
//!   sixth phase appears. A *false* document.
//! * **Ungoverned code** — behaviour no spec describes. Caught by [`coverage`],
//!   at crate granularity. An *incomplete* document.
//!
//! Statuses are honest ([`status`]): `Unknown` is first-class and never coerced
//! into `Ok`, and there is no headline score — the same discipline `sc-comply`
//! keeps, for the same reason.

pub mod anchor;
pub mod coverage;
pub mod engine;
pub mod manifest;
pub mod report;
pub mod resolve;
pub mod scan;
pub mod status;

#[cfg(test)]
mod test_support;

pub use anchor::{parse_anchors, Anchor, AnchorKind, SymbolRef};
pub use coverage::{ClaimKind, CrateCoverage};
pub use engine::{crate_coverage, trace, Claim, TraceReport, Ungoverned};
pub use manifest::{Crate, Workspace};
pub use resolve::{Located, Resolution};
pub use scan::{read_specs, SpecDoc};
pub use status::{ClaimStatus, Tally};
