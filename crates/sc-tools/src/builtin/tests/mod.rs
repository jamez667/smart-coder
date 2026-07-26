//! Tests for the built-in tool surface, split to mirror the implementation modules.
//!
//! The shared fixtures live here; each submodule covers one concern.

use std::path::PathBuf;

use crate::builtin::dispatch::ToolOutcome;
use crate::builtin::registry::default_registry;
use crate::spec::ValidatedCall;

mod guards;
mod read;
mod registry;
mod util;
mod write;

/// A unique temp workspace per test — pid + nanos, so parallel test threads never collide.
fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-tools-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Validate a raw tool-call JSON value against the default registry.
fn call(v: serde_json::Value) -> ValidatedCall {
    default_registry().validate(&v).unwrap()
}

/// Unwrap an [`ToolOutcome::Observation`], panicking on `Finished`.
fn obs(out: ToolOutcome) -> String {
    match out {
        ToolOutcome::Observation(o) => o,
        _ => panic!("expected observation"),
    }
}
