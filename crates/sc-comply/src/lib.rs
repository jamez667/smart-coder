//! Compliance evidence engine: framework packs in, an auditor-facing evidence
//! pack out.
//!
//! An evidence pack is an **argument, not a verdict**. The job of this crate is
//! to hand an auditor citations plus an honest map of what it could not see —
//! which is why [`ControlStatus::Unknown`](status::ControlStatus::Unknown) is a
//! first-class status, why there is no headline compliance percentage, and why
//! pack-driven commands are disabled by default.
//!
//! See `docs/specs/13-compliance-evidence.md`.

pub mod aggregate;
pub mod collector;
pub mod collectors;
pub mod engine;
pub mod evidence;
pub mod glob;
pub mod pack;
pub mod redact;
pub mod registry;
pub mod report;
pub mod rollup;
pub mod scan;
pub mod section;
pub mod status;

#[cfg(test)]
mod test_support;

pub use aggregate::{aggregate, Aggregate, WeightCfg};
pub use collector::{AuditContext, Collector, ComplyOptions, Observation, Registry};
pub use engine::{audit, audit_with};
pub use evidence::{
    CheckResult, ControlResult, Evidence, EvidencePack, Finding, FrameworkMeta, Score,
};
pub use glob::Glob;
pub use pack::{Assertion, Check, CheckKind, Control, Pack};
pub use registry::{load_shipped, ShippedPack, SHIPPED};
pub use scan::{scan_workspace, TextFile};
pub use section::Section;
pub use status::{ControlStatus, Outcome, OutcomePolicy, Severity};
