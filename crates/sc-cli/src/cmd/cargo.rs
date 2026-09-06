//! `cargo list | deps | rdeps` — the crate graph (spec 23).
//!
//! For humans, for scripts, and for debugging the tool: the text form is byte for
//! byte what `cargo_info` hands the model, so "why did it think that" is a
//! one-liner rather than an argument about what it was probably shown.
//!
//! `rdeps` has no model-facing twin. It is the question a person asks before
//! touching a shared crate -- what breaks if I change this -- and the answer is a
//! table nobody needs to spend a menu slot on.

use std::process::ExitCode;

use sc_cli::CargoAction;

use super::common::workspace;

/// Answer one question about the crate graph.
pub fn cargo(action: &CargoAction, json: bool) -> ExitCode {
    let Some(ws) = workspace() else {
        return ExitCode::FAILURE;
    };
    let graph = sc_cargo::Graph::load(&ws);
    if graph.is_empty() {
        eprintln!("cargo: no workspace members here — is this the root of a cargo workspace?");
        return ExitCode::FAILURE;
    }

    match action {
        CargoAction::List if json => println!("{}", json_list(&graph)),
        CargoAction::List => print!("{}", sc_cargo::list_view(&graph)),

        CargoAction::Deps { krate } => {
            if graph.get(krate).is_none() {
                return unknown(&graph, krate);
            }
            if json {
                println!("{}", json_crate(&graph, krate));
            } else {
                print!("{}", sc_cargo::crate_view(&graph, krate));
            }
        }

        CargoAction::Rdeps { krate } => {
            if graph.get(krate).is_none() {
                return unknown(&graph, krate);
            }
            if json {
                println!("{}", json_rdeps(&graph, krate));
            } else {
                print!("{}", sc_cargo::rdeps_view(&graph, krate));
            }
        }
    }
    ExitCode::SUCCESS
}

/// An unknown crate is a failure exit, but still names the real ones: this is
/// typed by a person at a terminal who should not have to go and read the help
/// text to recover from a typo.
fn unknown(graph: &sc_cargo::Graph, krate: &str) -> ExitCode {
    let names: Vec<&str> = graph.packages.iter().map(|p| p.name.as_str()).collect();
    eprintln!(
        "cargo: no crate named {krate:?}. Known crates: {}",
        names.join(", ")
    );
    ExitCode::FAILURE
}

fn json_list(graph: &sc_cargo::Graph) -> serde_json::Value {
    let rows: Vec<_> = graph
        .packages
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "dir": p.dir,
                "description": p.description,
            })
        })
        .collect();
    serde_json::json!({ "crates": rows })
}

fn json_crate(graph: &sc_cargo::Graph, name: &str) -> serde_json::Value {
    let Some(p) = graph.get(name) else {
        return serde_json::json!({});
    };
    let deps: Vec<_> = p
        .deps
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "internal": d.internal,
                "kind": d.kind.label(),
                "version": d.version,
            })
        })
        .collect();
    serde_json::json!({
        "name": p.name,
        "dir": p.dir,
        "description": p.description,
        "deps": deps,
        "features": p.features,
        "transitive": graph.transitive(name),
        "used_by": graph
            .dependents(name)
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>(),
    })
}

fn json_rdeps(graph: &sc_cargo::Graph, name: &str) -> serde_json::Value {
    let rows: Vec<_> = graph
        .dependents(name)
        .iter()
        .map(|p| {
            let kinds: Vec<&str> = p
                .deps
                .iter()
                .filter(|d| d.name == name)
                .map(|d| d.kind.label())
                .collect();
            serde_json::json!({ "name": p.name, "kinds": kinds })
        })
        .collect();
    serde_json::json!({ "crate": name, "dependents": rows })
}
