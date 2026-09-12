//! What a plugin declares: its identity, what it contributes, and what it needs.
//!
//! The [`Manifest`] arrives once, in the handshake reply, and is the **whole** of what
//! the plugin contributes. Panels and commands cannot be added later.
//!
//! That is a restriction worth justifying rather than apologising for. The host builds
//! its panel registry once at startup, before any layout is read, which is what lets
//! `PanelKind` stay `Copy` (an interned `u32` rather than a `String`) and what makes a
//! saved `layout.json` resolvable at all — a panel that could appear at any moment
//! would mean a layout whose leaves cannot be resolved when it is loaded. Spec 25 has
//! the argument.

use serde::{Deserialize, Serialize};

/// Everything a plugin contributes and needs, declared at handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Stable identifier: lowercase, `[a-z0-9-]`. Appears in panel slugs, so it is
    /// baked into `layout.json` and `splits.json` and **must not change** across
    /// versions of a plugin — see [`PanelDecl::slug`].
    pub id: String,

    /// Human name for the Plugins panel and menus.
    pub name: String,

    /// The plugin's own version, for display and bug reports. Not interpreted.
    #[serde(default)]
    pub version: String,

    /// The protocol version this plugin speaks.
    ///
    /// Checked against the host's before anything else. A version the host does not
    /// know is refused **by name** — "this plugin speaks protocol 3; this build
    /// understands 1" — rather than being discovered as a series of unparseable
    /// messages.
    pub protocol_version: u32,

    /// Panels this plugin contributes.
    #[serde(default)]
    pub panels: Vec<PanelDecl>,

    /// Commands this plugin contributes.
    #[serde(default)]
    pub commands: Vec<CommandDecl>,

    /// What the plugin wants the host to do for it.
    ///
    /// Requesting a capability the host lacks is not fatal: the host answers
    /// [`crate::ErrorCode::Unsupported`] and says so in the Plugins panel. A plugin
    /// that needs one to function should check `host_capabilities` in the handshake
    /// and degrade deliberately.
    #[serde(default)]
    pub capabilities: Vec<Capability>,

    /// Events the plugin wants delivered.
    ///
    /// Opt-in, because the expensive ones are the frequent ones. A plugin that does
    /// not subscribe to [`Subscription::SelectionChanged`] costs nothing when the
    /// cursor moves — which matters when the debounce still leaves several messages a
    /// second per subscriber.
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
}

/// A panel a plugin contributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelDecl {
    /// Stable within the plugin: lowercase, `[a-z0-9-]`.
    pub id: String,
    /// Shown in the panel header and the View menu.
    pub title: String,
}

impl PanelDecl {
    /// The slug this panel is persisted under: `plugin:<plugin id>:<panel id>`.
    ///
    /// **This string is load-bearing twice over.** It is the spelling in `layout.json`,
    /// and it is the seed for the divider keys in `splits.json` — which is why the
    /// layout module has a test asserting a single-pane layout "generates the split
    /// ids it always did". Changing the shape of this string silently resets every
    /// user's panel arrangement and every divider position.
    ///
    /// It is verbose, and it must stay verbose. Hashing or shortening it to look
    /// tidier would be the exact change that test exists to prevent. The `plugin:`
    /// prefix is what guarantees no collision with the host's own four split ids.
    pub fn slug(&self, plugin_id: &str) -> String {
        format!("plugin:{plugin_id}:{}", self.id)
    }
}

/// A command a plugin contributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDecl {
    /// Globally unique, conventionally `<plugin id>.<verb>`.
    ///
    /// A collision between two plugins is refused at handshake, naming both — the
    /// second plugin's command is dropped and the Plugins panel says so. Silently
    /// letting the later one win would make which plugin you get depend on directory
    /// ordering.
    pub id: String,
    /// Shown in menus and the command palette.
    pub title: String,
}

/// Something the host can do that a plugin may ask for.
///
/// Unknown values deserialize to [`Capability::Other`] rather than failing, so a plugin
/// built against a newer protocol still starts on an older host — it simply finds the
/// capability missing from `host_capabilities` and degrades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Read open buffers and list them.
    BufferRead,
    /// Apply edits to open buffers.
    BufferEdit,
    /// Read files from the workspace.
    FileRead,
    /// Publish into the Problems panel.
    Diagnostics,
    /// Open files in the editor.
    EditorOpen,
    /// Run host or plugin commands.
    RunCommand,
    /// A capability this build does not know.
    ///
    /// The forward-compatibility escape hatch: a v2 plugin declaring `decorations`
    /// lands here on a v1 host instead of failing to parse the whole manifest.
    #[serde(other)]
    Other,
}

/// An event stream a plugin can subscribe to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subscription {
    /// Buffers opened, saved, closed.
    BufferEvents,
    /// Buffer contents changed (version only, debounced).
    BufferChanged,
    /// Cursor or selection moved (debounced).
    SelectionChanged,
    /// The open project changed.
    WorkspaceChanged,
    /// A stream this build does not know. Ignored.
    #[serde(other)]
    Other,
}

/// Whether `id` is a usable plugin or panel identifier.
///
/// Lowercase ASCII, digits and hyphens, non-empty, not starting or ending with a
/// hyphen. Deliberately narrow: these strings end up inside `layout.json` slugs and
/// `splits.json` keys, where a colon would split a slug into the wrong parts and a
/// space would make the key unreadable in a file people hand-edit.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}
