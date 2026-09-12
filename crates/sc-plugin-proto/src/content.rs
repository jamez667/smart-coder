//! What a plugin can put in a panel: **a small declarative view model, not widgets**.
//!
//! # The bet this file makes
//!
//! Four content kinds — [`Content::List`], [`Content::Text`], [`Content::Form`],
//! [`Content::Stack`]. No pixels, no colours, no layout. A plugin says *what* it has;
//! the host decides how it looks.
//!
//! The alternative was exposing a widget tree, and it was rejected for a reason that
//! does not weaken over time: a widget tree is an API over `iced`, and `iced` is
//! pre-1.0 and moves. Taking it would bound this protocol's stability to `iced`'s,
//! which is not a promise anyone can make. Four kinds is a promise that *can* be kept.
//!
//! There is a second benefit that matters more than it sounds: a plugin panel renders
//! in the app's existing visual language for free. A plugin panel that looks foreign is
//! worse than no plugin panel.
//!
//! # When four is not enough
//!
//! It will not be enough for someone, and the pressure will be to leak renderer types
//! "just this once". The answer is to **version this model deliberately** — add a
//! variant, bump [`crate::PROTOCOL_VERSION`], keep the translation for old versions in
//! one place. Stating that here is cheaper than defending it later, which is the whole
//! reason this section exists.
//!
//! Spec 25 names this as the design decision most likely to be regretted. It is
//! written down so that a later reversal is a decision someone makes on purpose.

use serde::{Deserialize, Serialize};

/// A panel's content.
///
/// Unknown variants deserialize to [`Content::Unsupported`] rather than failing the
/// whole message, so a v2 plugin pushing a content kind this host has never heard of
/// renders a short honest placeholder instead of an empty panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Content {
    /// Rows, optionally clickable. The workhorse: diagnostics, file lists, search
    /// results, run feeds and chat threads are all lists of things.
    List { items: Vec<ListItem> },

    /// Markdown.
    ///
    /// Rendered through the host's existing markdown renderer, which is why this is a
    /// v1 kind rather than a v2 one — the capability already exists and is already
    /// used by the Claude panel.
    Text { markdown: String },

    /// Labelled inputs and buttons.
    ///
    /// The submit button sends [`crate::HostMessage::PanelEvent`] with the form's
    /// values encoded as `field=value` pairs, one per line. Deliberately a flat string
    /// rather than a typed map: a typed payload here is a second schema to version,
    /// and the plugin already knows what fields it declared.
    Form {
        fields: Vec<FormField>,
        /// The submit button's label. Absent ⇒ no submit button, i.e. a read-only
        /// display of current values.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<String>,
    },

    /// Vertical composition of the above.
    ///
    /// The only layout primitive, and intentionally the only one. Anything needing
    /// columns, tabs or nesting beyond this is asking for a widget tree by degrees.
    Stack { children: Vec<Content> },

    /// A content kind this host does not understand.
    ///
    /// The forward-compatibility escape hatch. The host renders a short note naming
    /// the plugin, rather than an empty panel the user cannot explain.
    #[serde(other)]
    Unsupported,
}

impl Content {
    /// An empty list — the natural "nothing yet" content, and what the host shows for
    /// a panel whose plugin has not pushed anything.
    pub fn empty() -> Self {
        Content::List { items: Vec::new() }
    }

    /// Total number of rendered elements, recursively.
    ///
    /// The host uses this to refuse absurd content before building a widget tree from
    /// it: a plugin that pushes 200,000 list items would otherwise freeze the UI
    /// thread, and a frozen editor is indistinguishable from a crashed one. Counting
    /// is cheap; rendering is not.
    pub fn element_count(&self) -> usize {
        match self {
            Content::List { items } => items.len(),
            Content::Text { .. } => 1,
            Content::Form { fields, .. } => fields.len(),
            Content::Stack { children } => {
                children.iter().map(Content::element_count).sum::<usize>() + 1
            }
            Content::Unsupported => 1,
        }
    }
}

/// One row of a [`Content::List`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    /// The row's main text.
    pub text: String,

    /// Secondary text, shown muted at the end of the row. A line count, a timestamp,
    /// an author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    /// A command to run when the row is clicked. Absent ⇒ the row is not clickable,
    /// and the host renders it as text rather than as a dead button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Arguments passed with `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl ListItem {
    /// A plain, non-clickable row.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            detail: None,
            command: None,
            args: Vec::new(),
        }
    }

    /// Add muted secondary text.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Make the row run a command when clicked.
    pub fn with_command(mut self, command: impl Into<String>, args: Vec<String>) -> Self {
        self.command = Some(command.into());
        self.args = args;
        self
    }
}

/// One field of a [`Content::Form`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    /// Identifies the field in the submitted `field=value` pairs.
    pub id: String,
    /// Shown beside the input.
    pub label: String,
    /// Current value.
    #[serde(default)]
    pub value: String,
    /// Greyed placeholder shown when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Render as a password field.
    ///
    /// Masks the display only. It is **not** a security property: the value still
    /// crosses the pipe in plain JSON, and a plugin is a subprocess with the user's
    /// full privileges anyway (spec 25 is direct about this). It exists so a key is
    /// not left legible on a shared screen.
    #[serde(default)]
    pub secret: bool,
}
