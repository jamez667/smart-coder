//! **The plugin wire protocol** (spec 25) — one definition, shared by the IDE host and
//! every plugin.
//!
//! # Why this is its own crate
//!
//! For the reason [`sc_proto`] already documents about itself: both ends share ONE
//! definition, and neither has to depend on the other to obtain it. A plugin linking
//! the IDE to learn the message shapes would pull in `iced`; the IDE linking a plugin
//! is not a thing that can happen at all. Restating the protocol on the far side is
//! exactly the drift spec 17 exists to catch, so it is prevented by construction
//! instead of detected later.
//!
//! The dependency list in `Cargo.toml` is deliberately two entries, and the comment
//! there says why: everything added here is imposed on every plugin, including
//! third-party ones.
//!
//! # The transport
//!
//! A plugin is a **child process**. The host writes [`HostMessage`] JSON to its stdin,
//! one object per line; the plugin writes [`PluginMessage`] JSON to its stdout, one
//! object per line. Line-delimited, because it is the framing the Claude Code
//! integration already uses successfully and needs no length prefixes to get right.
//!
//! Not a dynamic library: Rust has no stable ABI, panics cannot cross the boundary
//! safely, and `iced` types cannot cross it at all. Spec 25 has the full argument.
//!
//! # The rule that makes this survivable
//!
//! **An unparseable or unknown message is ignored, never fatal.** Both directions.
//! That rule is lifted verbatim from `sc_win::claudecode`, whose comment states the
//! reason better than a restatement would: the format belongs to another project and
//! will gain fields, so a session must not die because one line was unexpected.
//!
//! [`Incoming`] and [`Outgoing`] are the parse results that make this explicit —
//! neither has an "error" variant for a malformed line, only [`Incoming::Unknown`].
//! The host counts them, so the silence is reportable in the Plugins panel rather
//! than invisible.
//!
//! # Versioning
//!
//! [`PROTOCOL_VERSION`] is negotiated in the handshake. The host supports every
//! version it has ever shipped — a real and unbounded commitment, mitigated by
//! keeping v1 small and by keeping the translation for old versions in one place
//! rather than scattered through the host.

use serde::{Deserialize, Serialize};

pub mod content;
pub mod manifest;

pub use content::{Content, FormField, ListItem};
pub use manifest::{Capability, Manifest, PanelDecl};

/// The protocol version this crate defines.
///
/// A plugin declares the version it speaks in its [`manifest::Manifest`]; the host
/// refuses a version it does not know, by name, rather than failing on the first
/// message it cannot parse.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request id, correlating a request with its response.
///
/// Scoped to the sender: the host and the plugin each number their own requests from
/// 1, so `{"id": 3}` from the host and `{"id": 3}` from the plugin are unrelated.
/// Making them share a space would need a negotiated split, which buys nothing.
pub type RequestId = u64;

/// A buffer version, incremented by the host on every change to an open buffer.
///
/// **The whole point of this type is [`HostMessage::BufferEdit`]'s version check.** A
/// plugin computes an edit against text it read at some version; if the user has typed
/// since, applying it would corrupt the file in a way nothing can reconstruct. The
/// plugin sends back the version it saw, and a mismatch is refused.
///
/// This is the same hazard the editor's `save_conflict` already defends against on the
/// disk side, and the same answer: refuse, do not guess.
pub type Version = u64;

// ---------------------------------------------------------------------------
// Host → plugin
// ---------------------------------------------------------------------------

/// A message from the IDE to a plugin.
///
/// Three kinds, distinguished by what the plugin owes in reply:
///
/// * **Lifecycle** ([`Initialize`](HostMessage::Initialize), [`Shutdown`](HostMessage::Shutdown))
///   — the handshake and the orderly stop.
/// * **Notifications** (everything named `*Event` or `*Changed`) — no reply.
/// * **Responses** ([`Response`](HostMessage::Response), [`ErrorResponse`](HostMessage::ErrorResponse))
///   — answers to the plugin's own requests, carrying its `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum HostMessage {
    /// The handshake. First message on the wire, always.
    ///
    /// Carries what the plugin needs to decide whether it can run at all: the host's
    /// protocol version, the open workspace (`None` when no project is open — a real
    /// state, not an error), and the capabilities this host actually implements.
    ///
    /// The plugin replies [`PluginMessage::Initialized`] with its manifest.
    Initialize {
        protocol_version: u32,
        workspace: Option<String>,
        /// What the host will answer. A plugin asking for something absent here gets
        /// [`ErrorCode::Unsupported`], so it can degrade deliberately rather than
        /// discovering the gap one failed request at a time.
        host_capabilities: Vec<Capability>,
    },

    /// Stop. The plugin should flush and exit.
    ///
    /// The host then waits a bounded time before killing the process — a plugin that
    /// hangs on shutdown must not hang the editor's exit, which is the one moment a
    /// user is least willing to wait.
    Shutdown,

    /// A user interacted with one of the plugin's panels.
    ///
    /// `value` carries whatever the interacted element declared: the command id of a
    /// clicked [`ListItem`], the text of a submitted [`FormField`]. Deliberately a
    /// bare string — a typed payload here would be a second schema to version, and
    /// the plugin already knows what it rendered.
    PanelEvent { panel: String, value: String },

    /// A command the plugin declared was invoked, from a menu or the palette.
    CommandInvoked {
        command: String,
        /// Arguments, when invoked by a [`ListItem`] that carried them.
        #[serde(default)]
        args: Vec<String>,
    },

    /// A buffer was opened, saved, or closed.
    BufferEvent {
        event: BufferEventKind,
        path: String,
        version: Version,
    },

    /// A buffer's text changed.
    ///
    /// **Carries the version, never the content, and is debounced.** A per-keystroke
    /// notification with text means a 1000-line file is re-serialised to JSON on every
    /// keypress, for every subscribed plugin. A plugin that wants the text after a
    /// change asks for it with [`PluginMessage::BufferRead`].
    ///
    /// This codebase has already paid for the alternative twice, in different
    /// costumes: the file tree is cached because re-walking per frame made filtering
    /// laggy, and the workspace sync was slowed from 500ms to 2s because a steady drip
    /// of git spawns is felt as typing lag.
    BufferChanged { path: String, version: Version },

    /// The cursor or selection moved.
    ///
    /// Debounced like [`BufferChanged`](HostMessage::BufferChanged), and for the same
    /// reason: a plugin that shells out per selection (git blame is the motivating
    /// case) would otherwise spawn a process per cursor key.
    SelectionChanged {
        path: String,
        /// 1-based, matching `Diagnostic` and every other line number the user sees.
        line: usize,
        column: usize,
    },

    /// The open project changed, or was closed (`None`).
    WorkspaceChanged { workspace: Option<String> },

    /// A successful answer to one of the plugin's requests.
    Response {
        id: RequestId,
        #[serde(flatten)]
        payload: ResponsePayload,
    },

    /// A failed answer to one of the plugin's requests.
    ///
    /// Separate from [`Response`](HostMessage::Response) rather than an `Result` inside
    /// it, so a plugin that ignores errors has to ignore them *deliberately* — a
    /// silently-discarded error field is how a plugin ends up waiting forever for an
    /// answer it already received.
    ErrorResponse {
        id: RequestId,
        code: ErrorCode,
        message: String,
    },
}

/// What happened to a buffer. See [`HostMessage::BufferEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BufferEventKind {
    Opened,
    Saved,
    Closed,
}

/// The body of a successful [`HostMessage::Response`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum ResponsePayload {
    /// The text of a buffer or file, with the version it was read at.
    ///
    /// The version is what the plugin sends back in [`PluginMessage::BufferEdit`], so
    /// reading and editing are two halves of one exchange.
    Text { text: String, version: Version },
    /// The open buffers, most recently used first.
    Buffers { paths: Vec<String> },
    /// An edit was applied, and this is the version it produced.
    Edited { version: Version },
    /// The request succeeded and has nothing to return.
    Ok,
}

// ---------------------------------------------------------------------------
// Plugin → host
// ---------------------------------------------------------------------------

/// A message from a plugin to the IDE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PluginMessage {
    /// The answer to [`HostMessage::Initialize`]: everything the plugin contributes.
    ///
    /// The host learns the plugin's panels and commands here and **only** here. They
    /// cannot be added later, because the panel registry is built once at startup,
    /// before any layout is read — which is what lets `PanelKind` stay `Copy` and what
    /// makes a saved layout resolvable at all (spec 25).
    Initialized { manifest: Manifest },

    /// New content for one of the plugin's panels.
    ///
    /// **Pushed when the plugin's state changes**, not pulled per frame. The host
    /// caches the last content and repaints from cache; a synchronous round trip on
    /// the render path is the mistake the file-tree cache and the sync-interval
    /// change both exist to avoid.
    PanelContent { panel: String, content: Content },

    /// Replace this plugin's diagnostics for one file.
    ///
    /// **Replace, not append**, keyed by `(source, path)`. A plugin that publishes on
    /// every change while appending produces a growing pile of stale problems. An
    /// empty `diagnostics` clears the file.
    PublishDiagnostics {
        path: String,
        diagnostics: Vec<Diagnostic>,
    },

    /// Read an open buffer's in-memory text.
    BufferRead { id: RequestId, path: String },

    /// List the open buffers.
    BufferList { id: RequestId },

    /// Read a file from disk.
    ///
    /// Workspace-relative and refused outside the root. The host's existing
    /// editability rules apply — a binary, non-UTF-8 or oversized file is refused
    /// with [`ErrorCode::NotEditable`] rather than the host trying to JSON-encode it.
    FileRead { id: RequestId, path: String },

    /// Apply edits to an open buffer.
    ///
    /// `version` is the buffer version the plugin last saw; a mismatch is refused with
    /// [`ErrorCode::VersionConflict`] and nothing is applied.
    ///
    /// The edits apply **all or nothing**, with every position interpreted against the
    /// text as it was *before* any of them — partial application is unreconstructable,
    /// and positions that shift under earlier edits are a bug factory.
    ///
    /// They land on the undo stack as **one** entry, labelled with the plugin's name.
    /// A plugin edit the user cannot `Ctrl+Z` is a plugin edit the user will not
    /// forgive.
    BufferEdit {
        id: RequestId,
        path: String,
        version: Version,
        edits: Vec<Edit>,
    },

    /// Open a file in the editor, optionally at a line.
    EditorOpen {
        id: RequestId,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<usize>,
    },

    /// Run a command — the host's own, or another plugin's.
    RunCommand {
        id: RequestId,
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },

    /// Show a transient message to the user.
    Notify { level: NotifyLevel, message: String },

    /// Write to this plugin's log, shown in the Plugins panel.
    ///
    /// Plugins should use this rather than stdout, which is the protocol channel: a
    /// stray `println!` in a plugin becomes an unparseable line. Stderr is also
    /// captured, for exactly the cases where a plugin cannot help itself (a panic
    /// message, a linker error).
    Log { message: String },
}

/// One edit to a buffer. See [`PluginMessage::BufferEdit`].
///
/// Positions are **0-based** here, unlike the 1-based line numbers the user sees in
/// diagnostics — these are text coordinates, not display coordinates, and the
/// distinction is worth the inconsistency because an off-by-one in an edit corrupts a
/// file while an off-by-one in a diagnostic merely points at the wrong line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub start: Position,
    pub end: Position,
    /// The replacement. Empty deletes the range.
    pub text: String,
}

/// A 0-based text position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

/// A problem to show in the Problems panel.
///
/// Mirrors the host's own `Diagnostic` (file, 1-based line and column, severity,
/// optional code, message) because that model is already proven against real toolchain
/// output — which is the reason diagnostics are a v1 capability rather than a deferred
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// 1-based, as shown to the user.
    pub line: usize,
    /// 1-based.
    pub column: usize,
    pub severity: Severity,
    /// A machine-readable code (`E0433`, `no-unused-vars`), when the tool has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyLevel {
    Info,
    Warning,
    Error,
}

/// Why a request failed.
///
/// A closed set, because a plugin should be able to `match` on the reason and do
/// something different — retry after re-reading on [`VersionConflict`](ErrorCode::VersionConflict),
/// degrade permanently on [`Unsupported`](ErrorCode::Unsupported). A free-form string
/// would make every failure equally opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// No such buffer, file, panel or command.
    NotFound,
    /// The buffer changed since the version the plugin edited against. Re-read and
    /// recompute; do not retry the same edit.
    VersionConflict,
    /// Outside the workspace root, or otherwise refused.
    Forbidden,
    /// Binary, not UTF-8, or too large to hand over.
    NotEditable,
    /// This host does not implement the capability. Declared in
    /// [`HostMessage::Initialize`]'s `host_capabilities`, so a plugin can know this
    /// before asking.
    Unsupported,
    /// The request was malformed.
    BadRequest,
    /// Something went wrong inside the host.
    Internal,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// What one line from a plugin means to the host.
///
/// **There is no error variant**, and that is the design. A line that will not parse is
/// [`Unknown`](Incoming::Unknown), not a failure: the plugin may be a newer version
/// sending a message this host has never heard of, and killing the session over it
/// would make every protocol addition a breaking change.
///
/// The host counts `Unknown` and shows the count in the Plugins panel, so a plugin
/// silently talking past the host is visible rather than mysterious. That count is the
/// same instinct as the Claude driver's skipped-line counter, which exists because a
/// silently halved feed is worse than a loud failure.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A message this host understands.
    Message(Box<PluginMessage>),
    /// A line that did not parse, or a type this host does not know. Counted, ignored.
    Unknown,
}

/// What one line from the host means to a plugin. The mirror of [`Incoming`], with the
/// same rule and the same reason.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Message(Box<HostMessage>),
    Unknown,
}

/// Parse one line from a plugin.
///
/// Pure, so the whole format contract is provable on the host against recorded
/// fixtures with no child process — the property that makes `claudecode::parse_line`
/// testable, and the reason this is a free function rather than a method on a reader.
///
/// A blank line is [`Incoming::Unknown`] like any other unparseable line, rather than a
/// third variant: callers skip blanks before calling, and a blank that reaches here is
/// as unexpected as anything else.
pub fn parse_plugin_line(line: &str) -> Incoming {
    match serde_json::from_str::<PluginMessage>(line.trim()) {
        Ok(m) => Incoming::Message(Box::new(m)),
        Err(_) => Incoming::Unknown,
    }
}

/// Parse one line from the host. The mirror of [`parse_plugin_line`], used by plugins.
pub fn parse_host_line(line: &str) -> Outgoing {
    match serde_json::from_str::<HostMessage>(line.trim()) {
        Ok(m) => Outgoing::Message(Box::new(m)),
        Err(_) => Outgoing::Unknown,
    }
}

/// Serialize a message to one line, newline included, ready to write.
///
/// Infallible by construction: every type here derives `Serialize` over plain data with
/// no maps keyed by non-strings, so `to_string` cannot fail. The `unwrap_or_default`
/// is a formality that keeps the signature free of a `Result` no caller could act on —
/// a failure to serialise our own types is a bug to fix, not a runtime condition to
/// handle.
pub fn to_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests;
