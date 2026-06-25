//! `dc-tools` — the Tool Registry and built-in tools (spec 04).
//!
//! For a small model, the tool design *is* the reliability and safety story:
//! narrow, strongly-typed tools validated against strict schemas, so a malformed
//! call is caught and repaired before it ever touches the workspace. This crate
//! owns:
//!
//! * [`ToolSpec`]/[`ToolRegistry`] — the schemas and structured validation.
//! * the built-in v1 tools and their sandboxed execution ([`default_registry`],
//!   [`execute`]).
//!
//! The agent loop (`dc-core`) consumes a [`ToolRegistry`] rather than a hardcoded
//! enum, so the tool surface can grow without touching the loop (spec 04).

mod builtin;
mod grammar;
mod journal;
mod permission;
mod spec;

pub use builtin::{
    default_registry, execute, minimal_worker_registry, safe_join, source_files, ToolOutcome,
};
pub use grammar::{params_json_schema, registry_gbnf};
pub use journal::{EditRecord, Journal};
pub use permission::{Decision, PermissionPolicy};
pub use spec::{
    ParamSpec, ParamType, Permission, SideEffect, ToolRegistry, ToolSpec, ValidatedCall,
    ValidationError,
};
