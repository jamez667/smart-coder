//! One editor pane: its open tabs, which one is active, and the view state that belongs to
//! *looking at a file in this pane* rather than to the file itself.
//!
//! This is the same refactor [`super::tabs`] already did one level down, repeated one level up.
//! There, per-file state lived on `App` and switching tabs discarded it; here, per-pane state
//! lived on `App` and a second pane would have discarded the first pane's scroll, diff and
//! comment draft — or worse, painted one pane's diff over the other's lines.
//!
//! **The rule for deciding where something lives:** anything answering a question about *the
//! file set* — is anything dirty, is this path open, reload this path — iterates every pane.
//! Anything answering *what is the user looking at* reads the focused pane. State that carries
//! its own path (like `App::working`) stays global and filters itself.
//!
//! Spec 21.

use sc_win::layout::EditorId;

use super::Tab;

/// One editor pane.
pub(crate) struct EditorPane {
    /// The files open as tabs, left→right in the order they were opened.
    pub(crate) tabs: Vec<Tab>,
    /// The active tab's path.
    ///
    /// Still a path rather than an index into `tabs`, for the reason it always was: it is
    /// compared against git rows, `file_status` keys and plan paths all over the view, and an
    /// index would have to be resolved back to a path at every one of them.
    pub(crate) selected_file: Option<String>,

    // --- derived from the active tab, cached so `view()` never hits the disk ---
    /// The rendered contents of `selected_file` for the REVIEW view.
    pub(crate) code: Option<sc_win::CodeView>,
    /// Lines differing from HEAD in the shown file — the green diff wash and the minimap ticks.
    pub(crate) changed_lines: std::collections::BTreeSet<usize>,
    /// The full PR-style diff of the shown file.
    pub(crate) file_diff: sc_win::gitdiff::FileDiff,

    // --- viewport: two panes on the same file scroll independently ---
    pub(crate) code_scroll_y: f32,
    pub(crate) code_view_h: f32,
    pub(crate) code_view_w: f32,
    /// `(top, height)` as fractions — the minimap's "you are here" box.
    pub(crate) code_viewport: Option<(f32, f32)>,
    /// A line to scroll to once the newly-opened file has been laid out. Deferred because a
    /// same-frame scroll misses: the new content isn't in the layout yet.
    pub(crate) pending_scroll_line: Option<usize>,

    // --- the in-flight line comment, which is a gesture inside ONE pane ---
    pub(crate) comment_range: Option<(usize, usize)>,
    pub(crate) comment_draft: String,
    /// The active drag-select over line numbers: `(anchor, current)`.
    pub(crate) drag: Option<(usize, usize)>,
}

impl Default for EditorPane {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            selected_file: None,
            code: None,
            changed_lines: std::collections::BTreeSet::new(),
            file_diff: sc_win::gitdiff::FileDiff::default(),
            code_scroll_y: 0.0,
            // Generous first-frame guesses, replaced by the first real `on_scroll`. Zero here
            // would make the virtualizer render no lines until the user scrolled.
            code_view_h: 800.0,
            code_view_w: 900.0,
            code_viewport: None,
            pending_scroll_line: None,
            comment_range: None,
            comment_draft: String::new(),
            drag: None,
        }
    }
}

impl std::fmt::Debug for EditorPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorPane")
            .field("tabs", &self.tabs.len())
            .field("selected_file", &self.selected_file)
            .finish()
    }
}

impl EditorPane {
    /// The active tab.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn active_tab(&self) -> Option<&Tab> {
        let sel = self.selected_file.as_deref()?;
        self.tabs.iter().find(|t| t.path == sel)
    }

    /// The active tab, mutably.
    pub(crate) fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let sel = self.selected_file.clone()?;
        self.tabs.iter_mut().find(|t| t.path == sel)
    }

    /// Whether this pane holds a tab for `rel`.
    pub(crate) fn holds(&self, rel: &str) -> bool {
        self.tabs.iter().any(|t| t.path == rel)
    }

    /// Whether any tab here has unsaved edits.
    pub(crate) fn any_dirty(&self) -> bool {
        self.tabs.iter().any(|t| t.dirty)
    }
}

/// The panes an app has, keyed by id, with the focused one.
///
/// Wrapped in a type rather than left as loose fields on `App` so the invariant that makes the
/// cheap accessors safe — **there is always at least one pane** — is enforced in one place.
pub(crate) struct Panes {
    map: std::collections::BTreeMap<EditorId, EditorPane>,
    focused: EditorId,
    /// Monotonic. Ids are never reused: a stale id left in a queued `Task` or a persisted
    /// `PanelSlot` must resolve to nothing, not to whichever pane inherited the number.
    next: u32,
}

impl Default for Panes {
    fn default() -> Self {
        let mut map = std::collections::BTreeMap::new();
        map.insert(EditorId::FIRST, EditorPane::default());
        Self {
            map,
            focused: EditorId::FIRST,
            next: 1,
        }
    }
}

impl std::fmt::Debug for Panes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panes")
            .field("ids", &self.map.keys().collect::<Vec<_>>())
            .field("focused", &self.focused)
            .finish()
    }
}

impl Panes {
    /// The focused pane. Never `None`.
    ///
    /// Returning a reference rather than an `Option` is what keeps the ~37 call sites cheap; the
    /// price is the invariant that `map` is never empty, which [`Self::remove`] enforces. A
    /// `focused` id that somehow went stale falls back to the first pane rather than panicking —
    /// a wrong-but-present pane is recoverable, a crash is not.
    pub(crate) fn focused(&self) -> &EditorPane {
        self.map
            .get(&self.focused)
            .or_else(|| self.map.values().next())
            .expect("Panes always holds at least one pane")
    }

    /// The focused pane, mutably.
    pub(crate) fn focused_mut(&mut self) -> &mut EditorPane {
        if !self.map.contains_key(&self.focused) {
            // Repair rather than panic; see `focused`.
            if let Some(first) = self.map.keys().next().copied() {
                self.focused = first;
            }
        }
        self.map
            .get_mut(&self.focused)
            .expect("Panes always holds at least one pane")
    }

    /// Which pane is focused.
    pub(crate) fn focused_id(&self) -> EditorId {
        self.focused
    }

    /// Focus `id`, if it exists.
    pub(crate) fn focus(&mut self, id: EditorId) {
        if self.map.contains_key(&id) {
            self.focused = id;
        }
    }

    pub(crate) fn get(&self, id: EditorId) -> Option<&EditorPane> {
        self.map.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: EditorId) -> Option<&mut EditorPane> {
        self.map.get_mut(&id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&EditorId, &EditorPane)> {
        self.map.iter()
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Allocate a new pane and return its id.
    // First src caller lands with Phase 3's Split editor; the pane tests exercise it now.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert(&mut self) -> EditorId {
        let id = EditorId(self.next);
        self.next += 1;
        self.map.insert(id, EditorPane::default());
        id
    }

    /// Ensure a pane exists for `id`, allocating one if not.
    ///
    /// For a layout loaded from disk naming panes this session hasn't created yet: the saved
    /// arrangement is the authority on how many panes the user had, so we make them rather than
    /// pruning the tree down to what we happen to have.
    pub(crate) fn ensure(&mut self, id: EditorId) {
        self.map.entry(id).or_default();
        self.next = self.next.max(id.0 + 1);
    }

    /// Remove `id`, unless it is the last pane. Returns whether anything was removed.
    ///
    /// The last pane is never removed however empty it is — that is the same "never hide the
    /// last editor" rule `Layout::sanitize` enforces, and an IDE with nowhere to open a file is
    /// a broken window rather than a layout choice.
    pub(crate) fn remove(&mut self, id: EditorId) -> bool {
        if self.map.len() <= 1 || !self.map.contains_key(&id) {
            return false;
        }
        self.map.remove(&id);
        if self.focused == id {
            self.focused = *self.map.keys().next().expect("one pane remains");
        }
        true
    }

    /// Whether any pane holds a tab for `rel`, and which.
    ///
    /// The duplicate-file rule: a path lives in exactly ONE pane. `Tab` owns its buffer, so the
    /// same file open twice would be two independent buffers over one path — two dirty flags,
    /// two disk stamps, and one path-keyed save-conflict slot between them. Saving in one would
    /// raise a spurious conflict in the other whose only offered answer destroys the first
    /// pane's edits. Sharing one buffer across panes needs a document model the editor widget
    /// does not have, so until it does, opening a file already open elsewhere goes there.
    pub(crate) fn pane_holding(&self, rel: &str) -> Option<EditorId> {
        self.map
            .iter()
            .find(|(_, p)| p.holds(rel))
            .map(|(id, _)| *id)
    }

    /// Whether ANY pane has unsaved edits — the quit prompt and the window-title dot.
    pub(crate) fn any_dirty(&self) -> bool {
        self.map.values().any(|p| p.any_dirty())
    }

    /// Whether the tab for `rel` (wherever it lives) has unsaved edits.
    pub(crate) fn is_dirty(&self, rel: &str) -> bool {
        self.map
            .values()
            .any(|p| p.tabs.iter().any(|t| t.path == rel && t.dirty))
    }

    /// Drop every tab in every pane, and collapse to a single empty pane.
    ///
    /// For a workspace change: the old project's tabs are meaningless, and so is an arrangement
    /// of panes holding them.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_always_at_least_one_pane() {
        // The invariant behind `focused()` returning a reference rather than an Option — which
        // is what keeps the call sites from each growing a `let … else`.
        let mut panes = Panes::default();
        assert_eq!(panes.len(), 1);
        assert!(!panes.remove(EditorId::FIRST), "the last pane never goes");
        assert_eq!(panes.len(), 1);
    }

    #[test]
    fn ids_are_never_reused() {
        // A stale id must resolve to nothing rather than to whichever pane inherited the
        // number — otherwise a queued Task or a persisted PanelSlot silently addresses the
        // wrong pane.
        let mut panes = Panes::default();
        let a = panes.insert();
        assert!(panes.remove(a));
        let b = panes.insert();
        assert_ne!(a, b, "the closed pane's id was not handed out again");
        assert!(panes.get(a).is_none(), "and it resolves to nothing");
    }

    #[test]
    fn closing_the_focused_pane_retargets_focus() {
        let mut panes = Panes::default();
        let second = panes.insert();
        panes.focus(second);
        assert_eq!(panes.focused_id(), second);

        assert!(panes.remove(second));
        assert_eq!(
            panes.focused_id(),
            EditorId::FIRST,
            "focus followed to a pane that still exists"
        );
    }

    #[test]
    fn ensure_makes_room_for_a_saved_layouts_panes() {
        // A layout.json naming pane 7 is the authority on how many panes the user had; we make
        // them rather than pruning the tree to what this session happens to hold.
        let mut panes = Panes::default();
        panes.ensure(EditorId(7));
        assert!(panes.get(EditorId(7)).is_some());
        // And the allocator must not later hand out 7 again.
        assert!(panes.insert().0 > 7);
    }
}
