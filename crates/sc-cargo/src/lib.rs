//! `sc-cargo` — the crate graph (spec 23).
//!
//! Which crates exist, what each one depends on, and — the question no other
//! surface answers — **who depends on it**.
//!
//! This workspace has 22 crates and 88 internal dependency edges. That is past
//! the size where a reader, or a small model with a few thousand tokens of
//! context, can recover the shape by opening manifests one at a time. Worse, the
//! answers matter: spec 18 rests on *"`sc-server` depends on `sc-proto` and
//! nothing else"*, an invariant that until now could only be checked by eye.
//!
//! The commitment is the same one `sc-index` makes for symbols:
//!
//! > **The harness reads the manifests. The model reads a sentence.**
//!
//! Raw `cargo metadata` is the opposite of that — megabytes of resolved JSON,
//! actively hostile to a small context. So the value here is the *projection*,
//! not the data: a query in, a few hundred tokens out ([`render`]).
//!
//! ## Declared, not resolved
//!
//! Manifests are read directly rather than shelling out to `cargo metadata`.
//! That keeps this instant, offline, and free of any dependency — including a
//! working `cargo` on PATH, whose absence is a failure that shows up looking
//! like broken agent logic rather than a missing tool.
//!
//! The honest cost: these are **declared** dependencies. There are no transitive
//! external versions here and no feature unification. For the question this
//! answers — the shape of the workspace's own architecture — that is the right
//! trade, and for anything else `cargo metadata` is one shell command away.

pub mod manifest;
pub mod render;

#[cfg(test)]
mod tests;

use std::path::Path;

pub use manifest::{Dep, DepKind, Package};
pub use render::{crate_view, list_view, rdeps_view};

/// Every crate in the workspace, and the edges between them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    /// Members in manifest order — the order the workspace itself declares,
    /// which groups crates roughly by layer and is more useful to a reader than
    /// alphabetical.
    pub packages: Vec<Package>,
}

impl Graph {
    /// Read the workspace at `root`.
    ///
    /// Members come from `[workspace] members` rather than a directory walk, so a
    /// leftover directory never becomes a phantom crate — the same discipline
    /// `sc-trace` keeps, and for the same reason.
    pub fn load(root: &Path) -> Graph {
        let ws = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
        let dirs = manifest::members(&ws);
        let mut packages: Vec<Package> = dirs
            .iter()
            .map(|dir| manifest::read_package(root, dir))
            .collect();

        // Only now, with every member known, can a dependency be classified as
        // internal. One manifest on its own cannot tell.
        let names: Vec<String> = packages.iter().map(|p| p.name.clone()).collect();
        for p in &mut packages {
            for d in &mut p.deps {
                d.internal = names.contains(&d.name);
            }
        }
        Graph { packages }
    }

    /// One crate by name.
    pub fn get(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Crates that depend on `name`, in workspace order.
    ///
    /// The blast radius of a change: the question a reader actually has before
    /// touching a shared crate, and the one a manifest cannot answer because the
    /// edge is written at the other end.
    pub fn dependents(&self, name: &str) -> Vec<&Package> {
        self.packages
            .iter()
            .filter(|p| p.deps.iter().any(|d| d.name == name))
            .collect()
    }

    /// Every workspace crate reachable from `name` through normal dependencies,
    /// sorted, excluding `name` itself.
    ///
    /// Normal deps only: a dev-dependency is not part of what a crate links, so
    /// including it would overstate the build graph — which is precisely the
    /// claim spec 18 makes about `sc-server`.
    pub fn transitive(&self, name: &str) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        let mut queue: Vec<&str> = vec![name];
        while let Some(current) = queue.pop() {
            let Some(pkg) = self.get(current) else {
                continue;
            };
            for d in pkg.deps_of(DepKind::Normal).filter(|d| d.internal) {
                if !seen.contains(&d.name.as_str()) {
                    seen.push(&d.name);
                    queue.push(&d.name);
                }
            }
        }
        seen.retain(|n| *n != name);
        seen.sort_unstable();
        seen
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }
}
