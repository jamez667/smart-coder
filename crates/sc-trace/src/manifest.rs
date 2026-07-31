//! The workspace: which crates exist, and where each one's code lives.
//!
//! This is what makes the crate segment of a symbol anchor *reliable*. Anything
//! resolved against a real workspace member is grounded; anything that is not is
//! unambiguously wrong (spec 17).
//!
//! Members come from the workspace manifest rather than a directory listing, so a
//! leftover directory never becomes a phantom `UNGOVERNED` finding. Parsing is a
//! few lines of string handling rather than a `toml` dependency — the members
//! array is a flat list of quoted strings under one header, and `sc-comply` set
//! the precedent for preferring that to a dependency for a job this small.

use std::path::{Path, PathBuf};

/// One workspace member crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crate {
    /// The package name as written in the manifest: `sc-workflow`.
    pub name: String,
    /// The Rust path segment it is referred to by: `sc_workflow`.
    ///
    /// Cargo derives this by replacing `-` with `_` unless a `[lib] name`
    /// overrides it. No crate in this workspace overrides it; if one ever does,
    /// [`Workspace::load`] reads the override rather than assuming.
    pub lib_name: String,
    /// Workspace-relative directory: `crates/sc-workflow`.
    pub dir: String,
}

impl Crate {
    /// The crate's source root on disk.
    pub fn src_dir(&self, root: &Path) -> PathBuf {
        root.join(&self.dir).join("src")
    }
}

/// Every crate in the workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub crates: Vec<Crate>,
}

impl Workspace {
    /// Read the workspace manifest at `root/Cargo.toml`.
    ///
    /// A member whose own manifest cannot be read still yields a [`Crate`] with
    /// the conventional `-`→`_` lib name: the directory is the ground truth for
    /// *existence*, and dropping the member would silently un-govern it.
    pub fn load(root: &Path) -> Workspace {
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
        let crates = members(&manifest)
            .into_iter()
            .map(|dir| {
                let name = dir.rsplit('/').next().unwrap_or(&dir).to_string();
                let member_manifest =
                    std::fs::read_to_string(root.join(&dir).join("Cargo.toml")).unwrap_or_default();
                let name = package_name(&member_manifest).unwrap_or(name);
                let lib_name =
                    lib_name_override(&member_manifest).unwrap_or_else(|| name.replace('-', "_"));
                Crate {
                    name,
                    lib_name,
                    dir,
                }
            })
            .collect();
        Workspace { crates }
    }

    /// The crate a symbol anchor's first segment names (`sc_workflow`), if any.
    pub fn by_lib_name(&self, lib_name: &str) -> Option<&Crate> {
        self.crates.iter().find(|c| c.lib_name == lib_name)
    }

    /// The crate owning a workspace-relative path, if any. Longest directory
    /// wins so `crates/sc-comply-author/…` is not attributed to `sc-comply`.
    pub fn owning(&self, path: &str) -> Option<&Crate> {
        self.crates
            .iter()
            .filter(|c| path.starts_with(&format!("{}/", c.dir)))
            .max_by_key(|c| c.dir.len())
    }

    pub fn is_empty(&self) -> bool {
        self.crates.is_empty()
    }
}

/// The `members = [ … ]` paths from a workspace manifest.
///
/// Scoped to the `[workspace]` table so a `members` key elsewhere cannot be
/// mistaken for it, and comments are stripped so a commented-out member does not
/// become a phantom crate.
fn members(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_workspace = false;
    let mut in_members = false;
    for raw in manifest.lines() {
        let line = strip_comment(raw).trim();
        if line.starts_with('[') {
            in_workspace = line == "[workspace]";
            in_members = false;
            continue;
        }
        if !in_workspace {
            continue;
        }
        if !in_members {
            let Some(rest) = line.strip_prefix("members") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            in_members = true;
            // `members = ["a", "b"]` on one line is as valid as the block form.
            out.extend(quoted(rest));
            if rest.contains(']') {
                in_members = false;
            }
            continue;
        }
        out.extend(quoted(line));
        if line.contains(']') {
            in_members = false;
        }
    }
    out.into_iter().map(|m| m.replace('\\', "/")).collect()
}

/// `name = "sc-workflow"` from the `[package]` table.
fn package_name(manifest: &str) -> Option<String> {
    table_value(manifest, "[package]", "name")
}

/// `name = "…"` from a `[lib]` table, when a crate overrides the default.
fn lib_name_override(manifest: &str) -> Option<String> {
    table_value(manifest, "[lib]", "name")
}

/// The first `key = "value"` inside `table`.
fn table_value(manifest: &str, table: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for raw in manifest.lines() {
        let line = strip_comment(raw).trim();
        if line.starts_with('[') {
            inside = line == table;
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix('=') {
            return quoted(rest).into_iter().next();
        }
    }
    None
}

/// Every `"…"`-quoted string in a fragment.
fn quoted(fragment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = fragment;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        let value = &after[..end];
        if !value.is_empty() {
            out.push(value.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

/// Drop a trailing `#` comment, ignoring `#` inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[workspace]
resolver = "2"
members = [
    "crates/sc-proto",
    "crates/sc-workflow",   # a trailing comment
    # "crates/sc-ghost",    fully commented out
    "crates/sc-comply-author",
]

[workspace.package]
edition = "2021"
"#;

    #[test]
    fn reads_members_and_ignores_commented_out_ones() {
        let m = members(MANIFEST);
        assert_eq!(
            m,
            vec![
                "crates/sc-proto",
                "crates/sc-workflow",
                "crates/sc-comply-author"
            ]
        );
        // A commented-out member must not become a phantom crate that then
        // reports as ungoverned.
        assert!(!m.iter().any(|x| x.contains("ghost")), "{m:?}");
    }

    #[test]
    fn a_members_key_outside_the_workspace_table_is_ignored() {
        let manifest =
            "[package]\nmembers = [\"not-a-member\"]\n\n[workspace]\nmembers = [\"crates/real\"]\n";
        assert_eq!(members(manifest), vec!["crates/real"]);
    }

    #[test]
    fn the_inline_members_form_parses_too() {
        let manifest = "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\nresolver = \"2\"\n";
        assert_eq!(members(manifest), vec!["crates/a", "crates/b"]);
    }

    #[test]
    fn lib_names_default_to_the_package_name_with_underscores() {
        assert_eq!(
            package_name("[package]\nname = \"sc-workflow\"\nversion = \"0.0.0\"\n").as_deref(),
            Some("sc-workflow")
        );
        // No crate in this workspace overrides it today, but reading the
        // override rather than assuming is the same amount of code.
        assert_eq!(
            lib_name_override("[package]\nname = \"sc-x\"\n\n[lib]\nname = \"custom\"\n")
                .as_deref(),
            Some("custom")
        );
        assert_eq!(lib_name_override("[package]\nname = \"sc-x\"\n"), None);
    }

    #[test]
    fn loads_the_real_workspace_and_maps_lib_names() {
        // Against the actual repo: this is the mapping every symbol anchor's
        // first segment depends on.
        let root = crate::test_support::repo_root();
        let ws = Workspace::load(&root);
        assert!(ws.crates.len() > 15, "{:?}", ws.crates.len());

        let wf = ws
            .by_lib_name("sc_workflow")
            .expect("sc_workflow resolves to a member");
        assert_eq!(wf.name, "sc-workflow");
        assert_eq!(wf.dir, "crates/sc-workflow");
        assert!(wf.src_dir(&root).is_dir());

        // A name nobody publishes resolves to nothing — which is what makes an
        // unknown crate segment an unambiguous BROKEN.
        assert!(ws.by_lib_name("sc_imaginary").is_none());
    }

    #[test]
    fn owning_prefers_the_longest_matching_directory() {
        // `crates/sc-comply-author/...` must not be attributed to `sc-comply`.
        let ws = Workspace {
            crates: vec![
                Crate {
                    name: "sc-comply".into(),
                    lib_name: "sc_comply".into(),
                    dir: "crates/sc-comply".into(),
                },
                Crate {
                    name: "sc-comply-author".into(),
                    lib_name: "sc_comply_author".into(),
                    dir: "crates/sc-comply-author".into(),
                },
            ],
        };
        assert_eq!(
            ws.owning("crates/sc-comply-author/src/lib.rs")
                .unwrap()
                .name,
            "sc-comply-author"
        );
        assert_eq!(
            ws.owning("crates/sc-comply/src/lib.rs").unwrap().name,
            "sc-comply"
        );
        assert!(ws.owning("docs/specs/17-spec-traceability.md").is_none());
    }
}
