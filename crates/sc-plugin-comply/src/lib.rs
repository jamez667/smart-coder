//! **Compliance evidence, as a plugin** (spec 13 / spec 25).
//!
//! Audits the open project against every shipped framework and writes a redacted HTML
//! site — the same one the CLI's `comply-export` produces.
//!
//! # Why this is a plugin and not an editor feature
//!
//! The audit is **deterministic and model-free**: `sc-comply` has no `sc-model`
//! dependency, enforced by the crate graph rather than by a rule anyone has to remember,
//! so the same workspace always yields the same control results. On that basis it looked
//! like it belonged in the editor.
//!
//! What moved it out is the *optional* half. A model, when chosen, writes exactly two
//! things — the executive summary and auditor guidance for controls a code scan could not
//! settle — and neither can change a control's status. Keeping that in the editor would
//! have meant the editor keeping a path to a model, which is the one thing it must not
//! have (spec 21). The alternative was deleting the prose.
//!
//! So compliance became a plugin instead: the editor sheds `sc-model` and
//! `sc-comply-author`, and the summary keeps working. Nobody loses a feature to win a
//! dependency argument.

pub mod comply;
pub mod config;

pub use comply::{output_dir, ComplyError, ComplyModel, ComplyReport};
pub use config::{ComplyConfig, Connection, Provider};
