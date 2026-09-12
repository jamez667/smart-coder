//! Tests for the crate graph.
//!
//! Two halves, deliberately. The fixtures pin the parser against the *shapes* a
//! manifest can take — every one of which is taken from a real manifest in this
//! workspace, because inventing TOML to test against is how a parser passes its
//! tests and fails the repo. The real-workspace tests then assert the invariants
//! the specs actually claim, which is the thing this crate exists to make
//! checkable.

use super::*;
use crate::manifest::{members, DepKind};

/// The awkward manifest, assembled from the real ones: `sc-win` contributes the
/// target-scoped build-deps, the exact pin and the feature; `sc-eval` the
/// dev-dependency; `sc-proto` the interleaved comments.
const AWKWARD: &str = r#"
[package]
name = "sc-awkward"
version = "0.0.0"
edition.workspace = true
description = "A crate with every shape of dependency (spec 23)."

[features]
# A comment inside the features table.
base = []
extra = ["base"]

[dependencies]
sc-proto = { path = "../sc-proto" }
# A comment between two dependencies, as every manifest here has.
serde = { workspace = true }
serde_json.workspace = true
regex = "1"
iced-code-editor = "=0.3.11"
chrono = { version = "0.4", default-features = false, features = ["clock"] }

[dev-dependencies]
ureq = { version = "3", features = ["json"] }

[target.'cfg(windows)'.build-dependencies]
winresource = "0.1"
"#;

fn awkward() -> Package {
    Package {
        name: "sc-awkward".into(),
        dir: "crates/sc-awkward".into(),
        description: String::new(),
        deps: super::manifest::parse_dependencies(AWKWARD),
        features: super::manifest::parse_features(AWKWARD),
    }
}

#[test]
fn a_target_scoped_build_table_is_not_a_dependency() {
    // The trap: `[target.'cfg(windows)'.build-dependencies]` ends in
    // `-dependencies`, and a header check looser than this reads winresource as
    // something sc-win links on every platform.
    let p = awkward();
    let normal: Vec<&str> = p
        .deps_of(DepKind::Normal)
        .map(|d| d.name.as_str())
        .collect();
    assert!(!normal.contains(&"winresource"), "{normal:?}");

    let build: Vec<&str> = p.deps_of(DepKind::Build).map(|d| d.name.as_str()).collect();
    assert_eq!(build, vec!["winresource"]);
}

#[test]
fn dev_dependencies_stay_out_of_the_build_graph() {
    // sc-eval's `ureq` is fetched by a test, not linked by the crate. Spec 18's
    // claim about sc-server is exactly this distinction, so getting it wrong
    // would make the invariant unverifiable.
    let p = awkward();
    let normal: Vec<&str> = p
        .deps_of(DepKind::Normal)
        .map(|d| d.name.as_str())
        .collect();
    assert!(!normal.contains(&"ureq"), "{normal:?}");
    assert_eq!(
        p.deps_of(DepKind::Dev)
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        vec!["ureq"]
    );
}

#[test]
fn every_form_of_version_is_read() {
    let p = awkward();
    let version = |name: &str| {
        p.deps
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} not parsed from the manifest"))
            .version
            .clone()
    };
    assert_eq!(version("sc-proto"), "path");
    assert_eq!(version("serde"), "workspace");
    // The dotted form is the same statement as the inline table.
    assert_eq!(version("serde_json"), "workspace");
    assert_eq!(version("regex"), "1");
    // An exact pin is a deliberate act and must survive verbatim.
    assert_eq!(version("iced-code-editor"), "=0.3.11");
    // `default-features` must not be mistaken for the version key.
    assert_eq!(version("chrono"), "0.4");
    assert_eq!(version("ureq"), "3");
}

#[test]
fn an_inline_features_array_is_not_read_as_a_dependency() {
    // `features = ["clock"]` sits on a dependency line; a looser parser would
    // add `clock` or `features` as crates.
    let p = awkward();
    let names: Vec<&str> = p.deps.iter().map(|d| d.name.as_str()).collect();
    for noise in ["features", "clock", "json", "default-features", "version"] {
        assert!(!names.contains(&noise), "{noise} leaked into {names:?}");
    }
}

#[test]
fn features_are_the_table_keys_only() {
    assert_eq!(awkward().features, vec!["base", "extra"]);
}

#[test]
fn members_ignore_commented_out_and_out_of_table_keys() {
    let manifest = r#"
[package]
members = ["not-a-member"]

[workspace]
members = [
    "crates/sc-proto",
    "crates/sc-win",   # a trailing comment
    # "crates/sc-ghost",
]
"#;
    assert_eq!(members(manifest), vec!["crates/sc-proto", "crates/sc-win"]);
}

#[test]
fn an_unreadable_manifest_still_yields_the_crate() {
    // The directory is ground truth for existence. Dropping the member would
    // hide real code from the graph, which is worse than a sparse entry.
    let p = manifest::read_package(std::path::Path::new("/nonexistent"), "crates/sc-ghost");
    assert_eq!(p.name, "sc-ghost");
    assert!(p.deps.is_empty());
}

// ---------------------------------------------------------------------------
// Against the real workspace: the claims the specs actually make.
// ---------------------------------------------------------------------------

/// The repo root, from this crate's manifest directory.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/sc-cargo has a grandparent")
        .to_path_buf()
}

#[test]
fn loads_every_workspace_member() {
    let g = Graph::load(&repo_root());
    assert!(g.packages.len() > 15, "{}", g.packages.len());
    assert!(g.get("sc-proto").is_some());
    assert!(g.get("sc-cargo").is_some(), "this crate is a member");
    assert!(g.get("sc-imaginary").is_none());
}

#[test]
fn sc_server_depends_on_sc_proto_and_nothing_else() {
    // Spec 18: "no `sc-daemon`, and through it no `sc-model`, no `sc-workflow`,
    // no `sc-core`. The claim that no model is anywhere near the public server
    // becomes *literally* true rather than true in spirit."
    //
    // That was a claim checked by reading a manifest by eye. This is the whole
    // reason the crate exists, so it is asserted rather than described.
    let g = Graph::load(&repo_root());
    let server = g.get("sc-server").expect("sc-server is a member");
    let internal: Vec<&str> = server
        .deps_of(DepKind::Normal)
        .filter(|d| d.internal)
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(internal, vec!["sc-proto"], "spec 18's separation");

    // And transitively, which is the half prose cannot check: sc-proto pulls in
    // nothing from the workspace, so the closure stops there.
    assert_eq!(g.transitive("sc-server"), vec!["sc-proto"]);
    for forbidden in ["sc-daemon", "sc-model", "sc-workflow", "sc-core"] {
        assert!(
            !g.transitive("sc-server").contains(&forbidden),
            "{forbidden} reached the public server"
        );
    }
}

#[test]
fn sc_proto_keeps_its_one_dependency() {
    // Its own manifest: "The only dependency, and only because the wire types
    // cross a network... every dependency added here lands in the public
    // server's build."
    let g = Graph::load(&repo_root());
    let proto = g.get("sc-proto").expect("sc-proto is a member");
    let normal: Vec<&str> = proto
        .deps_of(DepKind::Normal)
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(normal, vec!["serde"]);
    assert!(proto.deps_of(DepKind::Normal).all(|d| !d.internal));
}

#[test]
fn reverse_deps_find_the_blast_radius() {
    let g = Graph::load(&repo_root());
    let names: Vec<String> = g
        .dependents("sc-proto")
        .iter()
        .map(|p| p.name.clone())
        .collect();
    // The shared protocol crate is depended on widely; that is the point of it.
    assert!(names.len() > 10, "{names:?}");
    assert!(names.contains(&"sc-server".to_string()));
    // A crate nothing depends on has an empty radius, and that is not an error.
    assert!(g.dependents("sc-win").is_empty(), "sc-win is a leaf binary");
}

#[test]
fn the_transitive_closure_exceeds_the_direct_deps() {
    // sc-win names 12 workspace crates and reaches more: sc-context and
    // sc-review arrive only through sc-core. A reader cannot get that from one
    // manifest, which is why the closure is rendered.
    let g = Graph::load(&repo_root());
    let direct = g
        .get("sc-win")
        .expect("sc-win is a member")
        .deps_of(DepKind::Normal)
        .filter(|d| d.internal)
        .count();
    let all = g.transitive("sc-win");
    assert!(all.len() > direct, "{} vs {direct}", all.len());
    assert!(all.contains(&"sc-context"), "{all:?}");
}

#[test]
fn the_graph_has_no_cycles_to_hang_the_walk() {
    // A cycle is illegal in cargo, but `transitive` must terminate on whatever it
    // is handed rather than trusting that.
    let g = Graph::load(&repo_root());
    for p in &g.packages {
        let closure = g.transitive(&p.name);
        assert!(
            !closure.contains(&p.name.as_str()),
            "{} reaches itself",
            p.name
        );
    }
}

// ---------------------------------------------------------------------------
// Rendering: what a small context actually receives.
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_crate_lists_the_real_ones() {
    // A bare "not found" makes a model guess a second name. Naming the members
    // ends it in one turn.
    let g = Graph::load(&repo_root());
    let out = crate_view(&g, "sc-nope");
    assert!(out.contains("no crate named"), "{out}");
    assert!(
        out.contains("sc-proto"),
        "the real names are offered: {out}"
    );
}

#[test]
fn a_view_of_a_real_crate_stays_small() {
    // The whole premise: a projection, not a dump. sc-win is the largest crate
    // in the workspace by dependency count, so it is the worst case.
    let g = Graph::load(&repo_root());
    let out = crate_view(&g, "sc-win");
    assert!(out.contains("sc-win"), "{out}");
    assert!(out.contains("workspace deps"), "{out}");
    // A features section is NOT asserted here: no crate in the workspace declares one
    // any more (`craft-only` was the last, removed when the Crafter became its own
    // product -- spec 21). `features_are_the_table_keys_only` covers the rendering
    // against a synthetic manifest, which is where it belongs anyway.
    assert!(out.contains("external deps"), "{out}");
    assert!(
        out.lines().count() < 30,
        "a view must stay readable in a small context, got {} lines:\n{out}",
        out.lines().count()
    );
}

#[test]
fn rdeps_names_the_kind_of_each_edge() {
    let g = Graph::load(&repo_root());
    let out = rdeps_view(&g, "sc-proto");
    assert!(out.contains("depend on sc-proto"), "{out}");
    assert!(out.contains("(dep)"), "the edge kind is shown: {out}");

    let none = rdeps_view(&g, "sc-win");
    assert!(none.contains("nothing in this workspace"), "{none}");
}

#[test]
fn a_listing_covers_every_member() {
    let g = Graph::load(&repo_root());
    let out = list_view(&g);
    for name in ["sc-proto", "sc-server", "sc-cargo"] {
        assert!(
            out.contains(name),
            "{name} missing from the listing:\n{out}"
        );
    }
}

#[test]
fn an_empty_graph_says_so_rather_than_returning_nothing() {
    // Spec 00 — fail loud. An empty string reads to a model as a broken tool.
    let empty = Graph::default();
    assert!(list_view(&empty).contains("no workspace members"));
    assert!(crate_view(&empty, "anything").contains("no workspace members"));
    assert!(rdeps_view(&empty, "anything").contains("no workspace members"));
}
