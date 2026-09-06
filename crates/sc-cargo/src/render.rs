//! Turning the graph into a few hundred tokens.
//!
//! This module is the point of the crate. The data is cheap to gather; what is
//! scarce is the context it gets rendered into, so every view here is a
//! projection chosen to answer one question and then stop.
//!
//! Conventions follow the built-in tools in `sc-tools`: the answer names the
//! query that produced it, output is deterministically ordered, the empty case is
//! a sentence rather than an empty string, and a long list is capped with the
//! remainder stated rather than silently truncated.

use crate::{DepKind, Graph, Package};

/// How many crates a listing shows before it stops. The workspace has 22; the cap
/// exists so a much larger workspace degrades into a countable summary rather
/// than eating the whole prompt.
const MAX_ROWS: usize = 40;

/// `list` — every crate, one line each.
pub fn list_view(graph: &Graph) -> String {
    if graph.is_empty() {
        return "cargo_info: no workspace members found (is this a cargo workspace?)".to_string();
    }
    let mut out = format!("{} crates:\n", graph.packages.len());
    for p in graph.packages.iter().take(MAX_ROWS) {
        out.push_str(&format!("  {}  {}", p.name, p.dir));
        if !p.description.is_empty() {
            out.push_str(&format!("\n      {}", first_sentence(&p.description)));
        }
        out.push('\n');
    }
    if graph.packages.len() > MAX_ROWS {
        out.push_str(&format!("  … {} more\n", graph.packages.len() - MAX_ROWS));
    }
    out
}

/// `<crate>` — what one crate is and what it depends on.
///
/// Internal dependencies come first and are labelled: they are the architecture,
/// and the thing a reader is nearly always asking about. External crates follow
/// as a single comma-joined line, because their names are all a reader needs and
/// one line each would triple the size of the answer.
pub fn crate_view(graph: &Graph, name: &str) -> String {
    let Some(p) = graph.get(name) else {
        return unknown_crate(graph, name);
    };
    let mut out = format!("{} ({})\n", p.name, p.dir);
    if !p.description.is_empty() {
        out.push_str(&format!("{}\n", p.description));
    }

    let internal: Vec<&str> = p
        .deps_of(DepKind::Normal)
        .filter(|d| d.internal)
        .map(|d| d.name.as_str())
        .collect();
    out.push('\n');
    if internal.is_empty() {
        out.push_str("workspace deps: none\n");
    } else {
        out.push_str(&format!("workspace deps ({}):\n", internal.len()));
        for d in &internal {
            out.push_str(&format!("  {d}\n"));
        }
        // The closure is the claim a reader usually wants to check: what this
        // crate pulls in overall, not just what it names directly.
        let all = graph.transitive(name);
        if all.len() > internal.len() {
            out.push_str(&format!("  (transitively: {})\n", all.join(", ")));
        }
    }

    let external: Vec<String> = p
        .deps_of(DepKind::Normal)
        .filter(|d| !d.internal)
        .map(|d| version_label(&d.name, &d.version))
        .collect();
    if external.is_empty() {
        out.push_str("external deps: none\n");
    } else {
        out.push_str(&format!(
            "external deps ({}): {}\n",
            external.len(),
            external.join(", ")
        ));
    }

    for (kind, label) in [(DepKind::Dev, "dev-deps"), (DepKind::Build, "build-deps")] {
        let names: Vec<String> = p
            .deps_of(kind)
            .map(|d| version_label(&d.name, &d.version))
            .collect();
        if !names.is_empty() {
            out.push_str(&format!("{label}: {}\n", names.join(", ")));
        }
    }

    if !p.features.is_empty() {
        out.push_str(&format!("features: {}\n", p.features.join(", ")));
    }

    let dependents = graph.dependents(name);
    out.push_str(&format!(
        "used by ({}): {}\n",
        dependents.len(),
        if dependents.is_empty() {
            "nothing in this workspace".to_string()
        } else {
            names_of(&dependents).join(", ")
        }
    ));
    out
}

/// `rdeps <crate>` — who depends on this, and how.
///
/// Separated from [`crate_view`] because "what breaks if I change this" is a
/// different question from "what is this", and answering it needs the dependency
/// KIND: a crate reached only through dev-dependencies is not in the shipped
/// build graph and is not at risk in the same way.
pub fn rdeps_view(graph: &Graph, name: &str) -> String {
    if graph.get(name).is_none() {
        return unknown_crate(graph, name);
    }
    let dependents = graph.dependents(name);
    if dependents.is_empty() {
        return format!("{name}: nothing in this workspace depends on it");
    }
    let mut out = format!("{} crate(s) depend on {name}:\n", dependents.len());
    for p in &dependents {
        let kinds: Vec<&str> = p
            .deps
            .iter()
            .filter(|d| d.name == name)
            .map(|d| d.kind.label())
            .collect();
        out.push_str(&format!("  {} ({})\n", p.name, kinds.join(", ")));
    }
    out
}

/// The error every view shares: name what was asked for, and what exists instead.
///
/// A bare "not found" makes a model guess again; listing the real names ends the
/// guessing in one turn.
fn unknown_crate(graph: &Graph, name: &str) -> String {
    if graph.is_empty() {
        return "cargo_info: no workspace members found (is this a cargo workspace?)".to_string();
    }
    let known = names_of(&graph.packages.iter().collect::<Vec<_>>());
    format!(
        "cargo_info: no crate named {name:?} in this workspace. Known crates: {}",
        known.join(", ")
    )
}

fn names_of(packages: &[&Package]) -> Vec<String> {
    packages.iter().map(|p| p.name.clone()).collect()
}

/// `serde 1` — the name, plus the pin when it says something.
///
/// A `path` or `workspace` pin is dropped for external crates in the joined list:
/// it is the same for nearly all of them and repeating it is noise.
fn version_label(name: &str, version: &str) -> String {
    if version.is_empty() || version == "path" || version == "workspace" {
        name.to_string()
    } else {
        format!("{name} {version}")
    }
}

/// The first sentence of a description, so a listing stays one line per crate.
/// Descriptions here routinely carry a spec reference in a trailing clause.
fn first_sentence(text: &str) -> &str {
    match text.find(". ") {
        Some(at) => &text[..=at],
        None => text,
    }
}
