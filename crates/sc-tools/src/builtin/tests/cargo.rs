//! `cargo_info` at the tool boundary.
//!
//! The graph itself is tested in `sc-cargo`. What is tested here is the contract
//! the model sees: that the schema accepts an omitted argument, that dispatch
//! reaches the executor, and that a wrong crate name comes back as a usable
//! observation rather than an error or an empty string.

use serde_json::json;

use super::{call, obs, temp_dir};
use crate::builtin::dispatch::execute;

/// A minimal two-crate workspace: enough for an edge to exist.
fn workspace(tag: &str) -> std::path::PathBuf {
    let ws = temp_dir(tag);
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n",
    )
    .unwrap();
    for (name, deps) in [
        ("alpha", "beta = { path = \"../beta\" }\nserde = \"1\"\n"),
        ("beta", ""),
    ] {
        let dir = ws.join("crates").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\ndescription = \"The {name} crate.\"\n\n\
                 [dependencies]\n{deps}"
            ),
        )
        .unwrap();
    }
    ws
}

#[test]
fn omitting_the_crate_lists_the_workspace() {
    // The optional parameter is the whole reason this is one tool and not two.
    let ws = workspace("cargo-list");
    let o = obs(execute(&call(json!({"tool":"cargo_info"})), &ws));
    assert!(o.contains("2 crates"), "got: {o}");
    assert!(o.contains("alpha"), "got: {o}");
    assert!(o.contains("beta"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn naming_a_crate_describes_it_and_its_edges() {
    let ws = workspace("cargo-one");
    let o = obs(execute(
        &call(json!({"tool":"cargo_info","crate":"alpha"})),
        &ws,
    ));
    assert!(o.contains("The alpha crate."), "the description: {o}");
    assert!(o.contains("beta"), "the workspace dep: {o}");
    // An external dependency must not be confused for a workspace one.
    assert!(o.contains("external deps"), "got: {o}");
    assert!(o.contains("serde"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_reverse_edge_is_visible_from_the_other_end() {
    // `beta` names nobody; the fact that `alpha` needs it is written in a file
    // beta cannot see. Surfacing that is the point of the tool.
    let ws = workspace("cargo-rdeps");
    let o = obs(execute(
        &call(json!({"tool":"cargo_info","crate":"beta"})),
        &ws,
    ));
    assert!(o.contains("workspace deps: none"), "got: {o}");
    assert!(o.contains("used by (1)"), "got: {o}");
    assert!(o.contains("alpha"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_unknown_crate_names_the_real_ones() {
    // Spec 00 — fail loud, and fail usefully: a bare "not found" costs the model
    // another turn to guess again.
    let ws = workspace("cargo-unknown");
    let o = obs(execute(
        &call(json!({"tool":"cargo_info","crate":"nope"})),
        &ws,
    ));
    assert!(o.contains("no crate named"), "got: {o}");
    assert!(o.contains("alpha"), "the real names are offered: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_directory_that_is_not_a_workspace_says_so() {
    // The filler-argument test in `registry.rs` drives every tool against a temp
    // dir; this must be an ordinary observation there, never a panic and never
    // an "internal: no executor".
    let ws = temp_dir("cargo-empty");
    let o = obs(execute(&call(json!({"tool":"cargo_info"})), &ws));
    assert!(o.contains("no workspace members"), "got: {o}");
    assert!(!o.starts_with("internal:"), "got: {o}");
    let _ = std::fs::remove_dir_all(&ws);
}
