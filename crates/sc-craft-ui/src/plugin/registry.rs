//! The panel registry: interned handles so `PanelKind` can stay `Copy`.
//!
//! # Why interning
//!
//! `PanelKind` is `Copy` today, and `panels()`, `contains`, `dedup`, `prune`, `slot_of`
//! and `menu_panels` all depend on it. A `PanelKind::Plugin(String)` would kill that and
//! touch every one of them and their tests.
//!
//! So a plugin panel is `PanelKind::Plugin(PluginPanelId)`, where the id is a `u32`
//! index into this registry, and the registry maps it back to
//! `(plugin id, panel id, title)`.
//!
//! # Why it can be a global
//!
//! `PanelKind::from_slug` is a pure free function called from deep inside
//! `Layout::parse`'s recursion. Threading a registry reference through it would touch
//! that whole path and ~25 tests.
//!
//! The registry is **written once, at startup, before any layout is read**, and is
//! read-only thereafter. That is a strictly narrower shape than `config::product()`,
//! which is already a process-global here — so this is a `OnceLock` rather than an
//! atomic, and cannot be flipped at all after it is set. The test-only setter is the
//! single exception, and it exists because tests must be able to register panels
//! without a running plugin.

use std::sync::OnceLock;

/// A plugin panel's handle. Small and `Copy`, which is the entire point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginPanelId(pub u32);

/// One registered plugin panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPanel {
    /// The plugin that contributed it.
    pub plugin_id: String,
    /// The panel's id within that plugin.
    pub panel_id: String,
    /// Shown in the header and the View menu.
    pub title: String,
}

impl RegisteredPanel {
    /// The persisted slug: `plugin:<plugin>:<panel>`.
    ///
    /// See `sc_plugin_proto::PanelDecl::slug` for why this string must never be
    /// shortened or hashed — it is the seed for `splits.json` divider keys as well as
    /// the spelling in `layout.json`.
    pub fn slug(&self) -> String {
        format!("plugin:{}:{}", self.plugin_id, self.panel_id)
    }
}

/// Every panel contributed by every loaded plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelRegistry {
    panels: Vec<RegisteredPanel>,
}

impl PanelRegistry {
    /// Register a panel, returning its handle.
    ///
    /// Registering the same `(plugin, panel)` twice returns the existing handle rather
    /// than a second one. Two handles for one panel would render it twice and break
    /// `dedup`, which identifies leaves by equality.
    pub fn register(&mut self, plugin_id: &str, panel_id: &str, title: &str) -> PluginPanelId {
        if let Some(i) = self
            .panels
            .iter()
            .position(|p| p.plugin_id == plugin_id && p.panel_id == panel_id)
        {
            return PluginPanelId(i as u32);
        }
        self.panels.push(RegisteredPanel {
            plugin_id: plugin_id.to_string(),
            panel_id: panel_id.to_string(),
            title: title.to_string(),
        });
        PluginPanelId(self.panels.len() as u32 - 1)
    }

    /// Look up a panel by handle.
    pub fn get(&self, id: PluginPanelId) -> Option<&RegisteredPanel> {
        self.panels.get(id.0 as usize)
    }

    /// Resolve a persisted slug back to a handle.
    ///
    /// `None` when the slug names a plugin that is not loaded — which is the whole
    /// mechanism by which a saved layout survives a disabled plugin. `Layout::parse`
    /// already treats a `None` leaf as "collapse the split onto its sibling", so this
    /// needs no new code path.
    pub fn from_slug(&self, slug: &str) -> Option<PluginPanelId> {
        let rest = slug.strip_prefix("plugin:")?;
        // `split_once` rather than `split(':')`: a panel id may not contain a colon
        // (`is_valid_id` forbids it), but splitting on the FIRST colon is still the
        // right rule, and it keeps this honest if that ever changes.
        let (plugin_id, panel_id) = rest.split_once(':')?;
        self.panels
            .iter()
            .position(|p| p.plugin_id == plugin_id && p.panel_id == panel_id)
            .map(|i| PluginPanelId(i as u32))
    }

    /// Every registered panel, in registration order.
    pub fn all(&self) -> impl Iterator<Item = (PluginPanelId, &RegisteredPanel)> {
        self.panels
            .iter()
            .enumerate()
            .map(|(i, p)| (PluginPanelId(i as u32), p))
    }

    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    pub fn len(&self) -> usize {
        self.panels.len()
    }
}

static REGISTRY: OnceLock<PanelRegistry> = OnceLock::new();

/// Install the registry. Called once, at startup, after every handshake and before any
/// layout is read.
///
/// A second call is ignored rather than panicking: the registry is read-only, so a
/// duplicate install is a bug that does no damage, and taking down the editor over it
/// would be worse than the bug.
pub fn install(registry: PanelRegistry) {
    let _ = REGISTRY.set(registry);
}

/// The registry, or an empty one before `install`.
///
/// Empty is the correct default rather than a panic: the Crafter with no plugins never
/// calls `install`, and every layout operation must still work.
pub fn registry() -> &'static PanelRegistry {
    static EMPTY: PanelRegistry = PanelRegistry { panels: Vec::new() };
    REGISTRY.get().unwrap_or(&EMPTY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_panel_round_trips_through_its_slug() {
        let mut r = PanelRegistry::default();
        let id = r.register("git-blame", "blame", "Blame");
        assert_eq!(r.get(id).unwrap().slug(), "plugin:git-blame:blame");
        assert_eq!(r.from_slug("plugin:git-blame:blame"), Some(id));
    }

    /// Registering twice returns one handle. Two handles for one panel would render it
    /// twice and defeat `dedup`, which identifies leaves by equality.
    #[test]
    fn registering_the_same_panel_twice_is_idempotent() {
        let mut r = PanelRegistry::default();
        let a = r.register("p", "x", "X");
        let b = r.register("p", "x", "X");
        assert_eq!(a, b);
        assert_eq!(r.len(), 1);
    }

    /// Two plugins may use the same panel id without colliding — the plugin id is part
    /// of the identity.
    #[test]
    fn the_same_panel_id_in_two_plugins_is_two_panels() {
        let mut r = PanelRegistry::default();
        let a = r.register("one", "main", "One");
        let b = r.register("two", "main", "Two");
        assert_ne!(a, b);
        assert_eq!(r.from_slug("plugin:two:main"), Some(b));
    }

    /// **The mechanism that lets a saved layout outlive a disabled plugin.** An
    /// unresolvable slug is `None`, which `Layout::parse` already handles by collapsing
    /// the split onto its sibling.
    #[test]
    fn an_unknown_slug_resolves_to_none() {
        let mut r = PanelRegistry::default();
        r.register("present", "p", "P");
        assert_eq!(r.from_slug("plugin:absent:p"), None);
        assert_eq!(r.from_slug("plugin:present:other"), None);
    }

    /// A slug that is not a plugin slug at all is `None`, so the host's own panel slugs
    /// can never be mistaken for plugin ones.
    #[test]
    fn a_host_panel_slug_is_not_a_plugin_slug() {
        let r = PanelRegistry::default();
        for host in [
            "files", "git", "editor", "bottom", "chat", "claude", "flame",
        ] {
            assert_eq!(r.from_slug(host), None, "{host} must not resolve");
        }
    }

    /// A malformed plugin slug is `None` rather than a panic — `layout.json` is a file
    /// people hand-edit.
    #[test]
    fn a_malformed_plugin_slug_is_none() {
        let r = PanelRegistry::default();
        for bad in ["plugin:", "plugin:only-one-part", "plugin::", "plugin"] {
            assert_eq!(r.from_slug(bad), None, "{bad:?} must not resolve");
        }
    }

    /// An empty registry resolves nothing and lists nothing — the shape the global
    /// takes before `install`, which is what the Crafter with no plugins runs with.
    ///
    /// Asserted against a local `PanelRegistry` rather than the global: calling
    /// `install` here would leak into every other test in this binary, since a
    /// `OnceLock` cannot be un-set.
    #[test]
    fn an_empty_registry_resolves_nothing() {
        let r = PanelRegistry::default();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert_eq!(r.all().count(), 0);
        assert_eq!(r.from_slug("plugin:any:thing"), None);
        assert_eq!(r.get(PluginPanelId(0)), None);
    }
}
