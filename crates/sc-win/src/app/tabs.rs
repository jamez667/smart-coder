//! Open editor tabs: one buffer per file, and the state that belongs to it.
//!
//! Before this existed, `open_tabs` was a `Vec<String>` of paths and *every* per-file field
//! (`code`, `code_scroll_y`, `file_diff`, …) lived on `App`, shared by whichever tab happened to
//! be selected. That is fine for a read-only viewer — switching tabs just reloads — but an
//! editor cannot work that way: shared state means switching tabs discards unsaved edits.
//!
//! So each tab owns its buffer. Spec 21.

use sc_win::editbuf::{Classified, DiskStamp, Ending, NoEdit};

/// Which of the CODE pane's two views a tab is showing.
///
/// The two are genuinely different surfaces, not one widget in two states: the review view
/// interleaves widgets *between* lines (removed-line rows, revert bars, inline comments) and
/// paints per-line washes, which no editor widget can do; the edit view has a caret. Trying to
/// make one serve both is where this design would fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TabView {
    /// Read-only: diffs, line comments, gates.
    Review,
    /// Editable.
    #[default]
    Edit,
}

/// Why a tab was opened — decides which view it lands in.
///
/// Opening from the tree means "I want to work on this"; opening from git or by following the
/// agent means "I want to see what changed". Guessing wrong is a minor annoyance either way,
/// which is why this is a default rather than a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The file tree → edit.
    Tree,
    /// The git panel, or the agent touching a file → review.
    Review,
}

impl Origin {
    fn view(self) -> TabView {
        match self {
            Origin::Tree => TabView::Edit,
            Origin::Review => TabView::Review,
        }
    }
}

/// A tab's editable content, or the reason it has none.
pub(crate) enum Buffer {
    /// A real editor over the file's full contents.
    Live(Box<iced_code_editor::CodeEditor>),
    /// Not editable — [`NoEdit::reason`] says why, shown where the caret would be. The review
    /// view still works, so the file is never simply unopenable.
    ReadOnly(NoEdit),
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `CodeEditor` is a large canvas-backed widget; dumping it would bury any struct
            // that contains a tab.
            Buffer::Live(_) => f.write_str("Live(..)"),
            Buffer::ReadOnly(r) => write!(f, "ReadOnly({r:?})"),
        }
    }
}

/// One open file.
pub(crate) struct Tab {
    /// Workspace-relative path — the tab's identity, and the key `selected_file` holds.
    pub(crate) path: String,
    pub(crate) buf: Buffer,
    pub(crate) view: TabView,
    /// Unsaved edits. Drives the tab-strip dot, the close prompt, and the save-conflict rule.
    pub(crate) dirty: bool,
    /// The line ending detected on load, restored on save.
    pub(crate) ending: Ending,
    /// The file began with a UTF-8 BOM.
    pub(crate) bom: bool,
    /// The file ended with a newline when it was opened.
    ///
    /// The editor widget's `content()` rejoins its lines and so **drops a trailing newline**.
    /// Nearly every source file has one, and dropping it rewrites the last line of every file
    /// you touch. Recorded here and restored on save.
    pub(crate) trailing_newline: bool,
    /// The file's fingerprint when it was opened or last saved. Compared against disk on every
    /// save to catch the agent writing underneath us.
    pub(crate) opened: DiskStamp,
}

impl std::fmt::Debug for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tab")
            .field("path", &self.path)
            .field("view", &self.view)
            .field("dirty", &self.dirty)
            .field("buf", &self.buf)
            .finish()
    }
}

impl Tab {
    /// Open `path` (workspace-relative) by reading `abs` from disk.
    ///
    /// Always returns a tab: an unreadable or un-editable file opens read-only with its reason
    /// rather than failing to open, so a click in the tree never does nothing.
    pub(crate) fn open(path: String, abs: &std::path::Path, origin: Origin) -> Self {
        let stamp = DiskStamp::read(abs).unwrap_or_default();
        let syntax = syntax_for(&path);
        let (buf, ending, bom, trailing_newline) = match std::fs::read(abs) {
            Err(_) => (
                Buffer::ReadOnly(NoEdit::Missing),
                Ending::default(),
                false,
                true,
            ),
            Ok(bytes) => match sc_win::editbuf::classify(&bytes) {
                Classified::Editable { text, ending, bom } => {
                    let trailing = text.ends_with('\n');
                    let mut ed = iced_code_editor::CodeEditor::new(&text, syntax);
                    ed.set_theme(editor_theme());
                    (Buffer::Live(Box::new(ed)), ending, bom, trailing)
                }
                Classified::Refused(why) => (Buffer::ReadOnly(why), Ending::default(), false, true),
            },
        };
        // A file that cannot be edited opens in the review view whatever the origin asked for —
        // an edit view with no caret and a refusal notice is worse than the surface that works.
        let view = match &buf {
            Buffer::ReadOnly(_) => TabView::Review,
            Buffer::Live(_) => origin.view(),
        };
        Self {
            path,
            buf,
            view,
            dirty: false,
            ending,
            bom,
            trailing_newline,
            opened: stamp,
        }
    }

    /// The live editor, if this tab has one.
    pub(crate) fn editor(&self) -> Option<&iced_code_editor::CodeEditor> {
        match &self.buf {
            Buffer::Live(e) => Some(e),
            Buffer::ReadOnly(_) => None,
        }
    }

    /// The live editor, mutably.
    pub(crate) fn editor_mut(&mut self) -> Option<&mut iced_code_editor::CodeEditor> {
        match &mut self.buf {
            Buffer::Live(e) => Some(e),
            Buffer::ReadOnly(_) => None,
        }
    }

    /// Whether this tab can be switched into the edit view at all.
    pub(crate) fn editable(&self) -> bool {
        matches!(self.buf, Buffer::Live(_))
    }

    /// The buffer's current text, or `None` when there is no editor.
    pub(crate) fn text(&self) -> Option<String> {
        self.editor().map(|e| e.content())
    }
}

/// The editor's colours.
///
/// Derived from the app's own theme rather than the crate's defaults, so the edit view sits in
/// the panel instead of looking like a widget dropped into it. Two deliberate choices:
///
/// * **The gutter matches the editor background exactly, with no border.** The crate ships a
///   contrasting gutter — its own tests assert the two differ — which reads as a stripe down the
///   left of every file. Line numbers are legible from their dimmer text alone.
/// * The canvas is [`crate::app::EDITOR_BG`], a shade darker than the panel around it, so the
///   document reads as recessed into the panel rather than flush with the chrome.
fn editor_theme() -> iced_code_editor::Style {
    let base = iced_code_editor::from_iced_theme(&iced::Theme::TokyoNight);
    iced_code_editor::Style {
        background: crate::app::EDITOR_BG,
        // Same colour, no separator — the gutter is part of the editor, not a column beside it.
        gutter_background: crate::app::EDITOR_BG,
        gutter_border: crate::app::EDITOR_BG,
        ..base
    }
}

/// The syntect syntax token for a path's extension, used for highlighting.
///
/// Falls back to `"txt"` (plain, no highlighting) rather than guessing — a wrong grammar
/// mis-colours the whole file, which reads as a rendering bug.
pub(crate) fn syntax_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rs",
        "py" => "py",
        "js" | "mjs" | "cjs" => "js",
        "ts" => "ts",
        "jsx" | "tsx" => "js",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "md",
        "html" | "htm" => "html",
        "css" => "css",
        "sh" | "bash" => "sh",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" => "cpp",
        "cs" => "cs",
        "go" => "go",
        "sql" => "sql",
        "xml" => "xml",
        _ => "txt",
    }
}

// Which tab activates after a close is `super::tab_after_close` — already written and tested
// against the old `Vec<String>` tabs. The rule didn't change, so it isn't re-implemented here.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_is_guessed_from_the_extension_and_falls_back_to_plain() {
        assert_eq!(syntax_for("src/app/mod.rs"), "rs");
        assert_eq!(syntax_for("Cargo.toml"), "toml");
        assert_eq!(syntax_for("README.md"), "md");
        assert_eq!(syntax_for("SRC/MAIN.RS"), "rs", "case-insensitive");
        // Unknown and extensionless files render plain rather than being mis-coloured by a
        // guessed grammar.
        assert_eq!(syntax_for("Makefile"), "txt");
        assert_eq!(syntax_for("data.qqq"), "txt");
    }

    #[test]
    fn an_unreadable_file_still_opens_read_only_with_a_reason() {
        // A click in the tree must never do nothing. Missing files open in the review view
        // carrying their explanation.
        let t = Tab::open(
            "gone.rs".to_string(),
            std::path::Path::new("/definitely/not/here/gone.rs"),
            Origin::Tree,
        );
        assert!(!t.editable(), "no buffer to type into");
        assert_eq!(t.view, TabView::Review, "forced to the view that works");
        assert!(matches!(t.buf, Buffer::ReadOnly(NoEdit::Missing)));
        assert!(!t.dirty);
    }
}
