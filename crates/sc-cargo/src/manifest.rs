//! Reading a `Cargo.toml`: the package, what it depends on, and what it turns on.
//!
//! Parsing is string handling rather than a `toml` dependency, following
//! `sc-trace` and `sc-comply`. The manifests in this workspace are the test:
//! comments interleave the dependency lines, versions arrive as bare strings and
//! as inline tables, and one crate carries a
//! `[target.'cfg(windows)'.build-dependencies]` header that must NOT be read as
//! `[dependencies]`.
//!
//! This deliberately duplicates a little of `sc_trace::manifest` rather than
//! sharing it. That module is the ground truth for spec-anchor resolution and
//! carries a spec anchor of its own; widening it to serve a second consumer would
//! put a documented, load-bearing contract at risk to save a dozen lines.

use std::path::Path;

/// Where a dependency was declared. A dev-dependency is not part of the shipped
/// build graph, so `sc-eval`'s `ureq` must never look like something the crate
/// links at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    /// `[dependencies]` — the real build graph.
    Normal,
    /// `[dev-dependencies]` — tests and examples only.
    Dev,
    /// `[build-dependencies]`, including target-scoped ones.
    Build,
}

impl DepKind {
    /// The word used when the kind has to be shown.
    pub fn label(self) -> &'static str {
        match self {
            DepKind::Normal => "dep",
            DepKind::Dev => "dev",
            DepKind::Build => "build",
        }
    }

    /// Which table header a kind is declared under, if any.
    ///
    /// Target-scoped tables are matched by their LAST segment, which is what
    /// keeps `[target.'cfg(windows)'.build-dependencies]` out of the ordinary
    /// dependency list while still being seen. A quoted cfg can itself contain a
    /// dot, so the split is from the right.
    fn of_header(header: &str) -> Option<DepKind> {
        let name = header.trim_start_matches('[').trim_end_matches(']');
        let last = name.rsplit('.').next().unwrap_or(name);
        match last {
            "dependencies" => Some(DepKind::Normal),
            "dev-dependencies" => Some(DepKind::Dev),
            "build-dependencies" => Some(DepKind::Build),
            _ => None,
        }
    }
}

/// One declared dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    /// The dependency name as written: `sc-proto`, `serde`.
    pub name: String,
    /// True when it resolves to another crate in this workspace.
    ///
    /// Set by [`crate::Graph::load`], which is the only place that knows the full
    /// member list; reading one manifest alone cannot tell.
    pub internal: bool,
    pub kind: DepKind,
    /// How the version was pinned, in a word: `0.8`, `=0.3.11`, `workspace`,
    /// `path`. Empty when the manifest said nothing useful.
    pub version: String,
}

/// One crate's manifest, reduced to what a reader needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Workspace-relative directory: `crates/sc-proto`.
    pub dir: String,
    /// The manifest's `description`, empty when it has none.
    pub description: String,
    /// Declared dependencies, sorted by (kind, name).
    pub deps: Vec<Dep>,
    /// Declared feature names, sorted.
    pub features: Vec<String>,
}

impl Package {
    /// Dependencies of one kind, in order.
    pub fn deps_of(&self, kind: DepKind) -> impl Iterator<Item = &Dep> {
        self.deps.iter().filter(move |d| d.kind == kind)
    }
}

/// Read one member's manifest.
///
/// A crate whose manifest cannot be read still yields a `Package` named after its
/// directory: the directory is ground truth for existence, and dropping it would
/// silently hide a crate from the graph.
pub fn read_package(root: &Path, dir: &str) -> Package {
    let text = std::fs::read_to_string(root.join(dir).join("Cargo.toml")).unwrap_or_default();
    let fallback = dir.rsplit('/').next().unwrap_or(dir).to_string();
    Package {
        name: table_value(&text, "[package]", "name").unwrap_or(fallback),
        dir: dir.to_string(),
        description: table_value(&text, "[package]", "description").unwrap_or_default(),
        deps: parse_dependencies(&text),
        features: parse_features(&text),
    }
}

/// The `members = [ … ]` paths from a workspace manifest.
///
/// Scoped to the `[workspace]` table so a `members` key elsewhere cannot be
/// mistaken for it, and comments are stripped so a commented-out member does not
/// become a phantom crate.
pub fn members(manifest: &str) -> Vec<String> {
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

/// Every dependency declared in a manifest, from all three kinds of table.
///
/// `internal` is left false here — only the workspace member list can decide it.
pub(crate) fn parse_dependencies(manifest: &str) -> Vec<Dep> {
    let mut out: Vec<Dep> = Vec::new();
    let mut kind: Option<DepKind> = None;
    for raw in manifest.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            kind = DepKind::of_header(line);
            continue;
        }
        let Some(kind) = kind else { continue };
        let Some((name, rhs)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        // `serde.workspace = true` is the dotted form of
        // `serde = { workspace = true }`: take the crate name, keep the key.
        let (name, dotted) = match name.split_once('.') {
            Some((n, key)) => (n.trim(), Some(key.trim())),
            None => (name, None),
        };
        if !is_bare_name(name) {
            continue;
        }
        out.push(Dep {
            name: name.to_string(),
            internal: false,
            kind,
            version: version_of(rhs.trim(), dotted),
        });
    }
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    out.dedup_by(|a, b| a.name == b.name && a.kind == b.kind);
    out
}

/// How a dependency was pinned, in a couple of words.
///
/// What matters to a reader is *where the version comes from* — a path dep is
/// local, a workspace dep is centralised, an exact pin is deliberate — rather
/// than the semver string on its own.
fn version_of(rhs: &str, dotted: Option<&str>) -> String {
    if dotted == Some("workspace") {
        return "workspace".to_string();
    }
    if !rhs.starts_with('{') {
        // A bare string: `regex = "1"`.
        return quoted(rhs).into_iter().next().unwrap_or_default();
    }
    if inline_key(rhs, "path").is_some() {
        return "path".to_string();
    }
    if inline_flag(rhs, "workspace") {
        return "workspace".to_string();
    }
    inline_key(rhs, "version").unwrap_or_default()
}

/// The `[features]` table's key names.
///
/// Only the names: what a feature *enables* is a second question, and a small
/// context is better served by "these exist" than by the full expansion.
pub(crate) fn parse_features(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for raw in manifest.lines() {
        let line = strip_comment(raw).trim();
        if line.starts_with('[') {
            inside = line == "[features]";
            continue;
        }
        if !inside || line.is_empty() {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            let name = name.trim();
            if is_bare_name(name) {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
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

/// `key = "value"` inside an inline table fragment.
fn inline_key(fragment: &str, key: &str) -> Option<String> {
    let at = find_key(fragment, key)?;
    let rest = fragment[at + key.len()..].trim_start().strip_prefix('=')?;
    quoted(rest).into_iter().next()
}

/// `key = true` inside an inline table fragment.
fn inline_flag(fragment: &str, key: &str) -> bool {
    let Some(at) = find_key(fragment, key) else {
        return false;
    };
    let Some(rest) = fragment[at + key.len()..].trim_start().strip_prefix('=') else {
        return false;
    };
    rest.trim_start().starts_with("true")
}

/// Where `key` appears as a whole word in a fragment.
///
/// Whole-word so `version` is not found inside `default-features`, and `path` not
/// inside a quoted `"../path/to"` — the bug a bare `find` would introduce.
fn find_key(fragment: &str, key: &str) -> Option<usize> {
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let mut from = 0;
    while let Some(rel) = fragment[from..].find(key) {
        let at = from + rel;
        let before_ok = fragment[..at].chars().next_back().is_none_or(boundary);
        let after_ok = fragment[at + key.len()..]
            .chars()
            .next()
            .is_none_or(boundary);
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + key.len();
    }
    None
}

/// Whether a bare key looks like a crate/feature name rather than manifest
/// furniture. Guards against reading a continuation line of a multi-line inline
/// table (`default-features = false`) as if it were a dependency.
fn is_bare_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
