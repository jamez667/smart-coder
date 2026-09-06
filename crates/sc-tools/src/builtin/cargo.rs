//! `cargo_info` — the crate graph, as an observation.
//!
//! A thin adapter: `sc-cargo` reads the manifests and renders them, and this
//! turns a validated call into the right view. The interesting decisions all live
//! in that crate; what belongs here is the tool's contract — an omitted argument
//! means "list them all", and an unknown crate is an observation naming the real
//! ones rather than an error.

use std::path::Path;

/// Answer a `cargo_info` call.
///
/// `krate` is `None` when the model omitted the optional parameter, which is the
/// "what is in this workspace" question. Anything else is a request about one
/// crate.
pub fn cargo_info(workspace: &Path, krate: Option<&str>) -> String {
    let graph = sc_cargo::Graph::load(workspace);
    match krate.map(str::trim).filter(|s| !s.is_empty()) {
        None => sc_cargo::list_view(&graph),
        Some(name) => sc_cargo::crate_view(&graph, name),
    }
}
