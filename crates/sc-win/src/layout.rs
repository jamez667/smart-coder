//! The arrangeable panel tree: which panels are on screen, and how they're split.
//!
//! Pure and host-testable — the tree is data, so validation, defaults and persistence can all be
//! tested without a window.
//!
//! Before this existed, `view()` hardcoded three children in a row with the explorer pinned at
//! 20% and literal geometry in the drag maths. Panel identity was *positional*, which is why
//! "hide the chat column" was a special case in a layout made only of special cases. Here a
//! layout is a tree, so hiding a panel is an edit to data rather than a branch in the view.
//!
//! **The fraction is deliberately NOT stored in the tree.** A node carries a stable `id` and the
//! fraction lives in [`crate::splits::SplitStore`], which already persists `id → f32` with
//! NaN/range rejection. That keeps [`Layout`] a pure `Eq` structure (so tests can compare whole
//! trees) and means an upgrading user's saved divider positions load straight into the new
//! layout with no migration code. Spec 21.

use std::collections::BTreeSet;

/// Which editor pane. Panes are told apart by this and nothing else.
///
/// **Ids are never reused.** A monotonically-allocated id means a stale one — left in a queued
/// `Task`, or in a [`PanelSlot`] persisted before a pane was closed — resolves to nothing rather
/// than silently addressing a *different* pane that happens to have inherited the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EditorId(pub u32);

impl EditorId {
    /// The pane that exists in every layout — the one a single-pane user has, and the one a
    /// layout written before panes existed resolves to.
    pub const FIRST: EditorId = EditorId(0);
}

impl std::fmt::Display for EditorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A panel that can appear in the tree.
///
/// The `Assistant`-only ones are marked: a Craft-mode layout containing one is not an error, it
/// is simply pruned (see [`Layout::prune`]) — the same "fall back, never wedge" rule
/// [`crate::splits::SplitStore`] follows for a corrupt fraction.
///
/// [`PanelKind::Editor`] carries an [`EditorId`] because editor panes are the one kind you can
/// have several of: each owns its own tabs and scroll, so two of them are two different things
/// on screen. Every other panel renders one piece of app state, so a second would be the same
/// view twice — see [`Layout::dedup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PanelKind {
    /// The workspace file tree.
    Files,
    /// Branch, sync bar, and changed files.
    Git,
    /// A CODE pane — editor or review — with its own tabs.
    Editor(EditorId),
    /// The bottom strip (Problems / Terminal / …).
    Bottom,
    /// The chat thread and composer. **Assistant only.**
    Chat,
}

impl PanelKind {
    /// The stable slug persisted in `layout.json` — and, via [`Layout::move_panel`] and
    /// [`Layout::move_to_edge`], baked into the **split ids that key
    /// [`crate::splits::SplitStore`]**.
    ///
    /// The first editor pane is spelled `"editor"`, exactly as it was before panes had ids.
    /// That is load-bearing, not cosmetic: a single-pane layout must serialise byte-identically
    /// to what earlier builds wrote, and its split ids (`chat|editor`, `edge:editor:right`) must
    /// keep matching the divider positions already saved under them. Emitting `editor:0` would
    /// silently reset every existing user's dividers. Only a *second* pane introduces a new
    /// spelling, and only it can introduce new ids.
    pub fn slug(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            PanelKind::Files => Cow::Borrowed("files"),
            PanelKind::Git => Cow::Borrowed("git"),
            // `EditorId::FIRST` can't appear in a pattern (associated consts aren't patterns),
            // hence the literal — kept in step by `editor_zero_is_still_spelled_editor`.
            PanelKind::Editor(EditorId(0)) => Cow::Borrowed("editor"),
            PanelKind::Editor(EditorId(n)) => Cow::Owned(format!("editor:{n}")),
            PanelKind::Bottom => Cow::Borrowed("bottom"),
            PanelKind::Chat => Cow::Borrowed("chat"),
        }
    }

    /// Parse a slug. Unknown ⇒ `None`, which [`Layout::parse`] prunes rather than failing on.
    ///
    /// Accepts both editor spellings: bare `"editor"` (every layout written before panes had
    /// ids) and `"editor:N"`.
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim() {
            "files" => Some(PanelKind::Files),
            "git" => Some(PanelKind::Git),
            "editor" => Some(PanelKind::Editor(EditorId::FIRST)),
            "bottom" => Some(PanelKind::Bottom),
            "chat" => Some(PanelKind::Chat),
            rest => rest
                .strip_prefix("editor:")
                .and_then(|n| n.parse().ok())
                .map(|n| PanelKind::Editor(EditorId(n))),
        }
    }

    /// Short label — the panel's drag header, and the drag ghost.
    ///
    /// Every editor pane is just "Editor" here: the header names what the panel *is*, and a
    /// number would be noise on the common single-pane layout. [`Self::menu_label`] disambiguates
    /// where it matters.
    pub fn label(self) -> &'static str {
        match self {
            PanelKind::Files => "Files",
            PanelKind::Git => "Source control",
            PanelKind::Editor(_) => "Editor",
            PanelKind::Bottom => "Panel",
            PanelKind::Chat => "Chat",
        }
    }

    /// Label for the View menu, where several editors can be listed at once and have to be told
    /// apart. Numbered from 1 for humans; the first is plain "Editor".
    pub fn menu_label(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            PanelKind::Editor(EditorId(0)) => Cow::Borrowed("Editor"),
            PanelKind::Editor(EditorId(n)) => Cow::Owned(format!("Editor {}", n + 1)),
            other => Cow::Borrowed(other.label()),
        }
    }

    /// Whether this panel needs the agent. Craft mode prunes these.
    pub fn needs_model(self) -> bool {
        matches!(self, PanelKind::Chat)
    }

    /// Whether this is an editor pane, whichever one.
    pub fn is_editor(self) -> bool {
        matches!(self, PanelKind::Editor(_))
    }
}

/// The panels the View menu offers, for a given layout.
///
/// Replaces a fixed `ALL` array: how many editor panes exist is a property of the user's
/// layout, not of the type. Editors come from the tree (so each is listed once, in tree order);
/// the singleton panels are always offered, whether or not they're currently shown, because the
/// menu is how you get a hidden one back.
pub fn menu_panels(layout: &Layout) -> Vec<PanelKind> {
    let mut out = vec![PanelKind::Files, PanelKind::Git];
    out.extend(layout.editor_ids().into_iter().map(PanelKind::Editor));
    // A layout with no editor is rejected by `sanitize`, but a caller could hold one mid-edit;
    // offer the first pane so the menu is never editor-less.
    if !out.iter().any(|k| k.is_editor()) {
        out.push(PanelKind::Editor(EditorId::FIRST));
    }
    out.push(PanelKind::Bottom);
    out.push(PanelKind::Chat);
    out
}

/// Which way a split divides its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Side by side; the fraction is the left child's share of the width.
    Horizontal,
    /// Stacked; the fraction is the top child's share of the height.
    Vertical,
}

impl Axis {
    pub fn slug(self) -> &'static str {
        match self {
            Axis::Horizontal => "h",
            Axis::Vertical => "v",
        }
    }
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim() {
            "h" => Some(Axis::Horizontal),
            "v" => Some(Axis::Vertical),
            _ => None,
        }
    }
}

/// A panel arrangement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// One panel.
    Leaf(PanelKind),
    /// Two children with a draggable divider between them. `id` keys the divider's position in
    /// [`crate::splits::SplitStore`].
    Split {
        id: String,
        axis: Axis,
        a: Box<Layout>,
        b: Box<Layout>,
    },
}

/// Which edge of a panel a dragged panel is dropped against.
///
/// The drop target is an edge rather than a whole panel because "put this next to that" is
/// ambiguous — left or right, above or below — and a layout tool that guesses will guess wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    /// A stable name, used to key the split a drop creates.
    pub fn slug(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    /// The axis a drop on this side creates.
    pub fn axis(self) -> Axis {
        match self {
            Side::Left | Side::Right => Axis::Horizontal,
            Side::Top | Side::Bottom => Axis::Vertical,
        }
    }

    /// Whether the dropped panel becomes the first (left/top) child.
    pub fn puts_new_first(self) -> bool {
        matches!(self, Side::Left | Side::Top)
    }

    /// How close to an edge a drop must be, as a fraction of the panel, to mean "span the whole
    /// layout" rather than "split this panel".
    ///
    /// Generous on purpose. This is only ever consulted while a drag is in flight, so there is
    /// no cost to a large target — nothing else competes for those pixels — and a fiddly docking
    /// zone is the difference between a feature people use and one they give up on. A quarter of
    /// the panel still leaves the middle half unambiguously "split this one".
    pub const EDGE_BAND: f32 = 0.25;

    /// Whether a drop at this point should span the WHOLE layout rather than split the panel.
    ///
    /// True only when the cursor is inside [`Self::EDGE_BAND`] of the panel's edge *and* that
    /// edge is also an outer edge of the layout — a panel in the middle has no outer edge, so
    /// its bands mean nothing.
    ///
    /// `x`/`y` are the cursor within the panel, `w`/`h` the panel's size, and `tw`/`th` the
    /// **panel tree's** size — NOT the window's.
    ///
    /// The comparison is *spans*, not coordinates: a panel is at the tree's edge exactly when it
    /// reaches across the tree on that axis, which needs no origin at all. That sidesteps the
    /// bug this signature exists to prevent — the tree sits below a menu bar and sometimes above
    /// a gate bar, so a window-relative test could never fire on the top or bottom, and an
    /// origin derived by watching panels would only be right after enough of them were hovered.
    pub fn is_outer(self, x: f32, y: f32, w: f32, h: f32, tw: f32, th: f32) -> bool {
        // A panel never spans the tree exactly: it loses height to its drag header, and both axes
        // to padding, dividers and card borders. This has to absorb all of that — being a few
        // pixels too strict presents to the user as "the snap doesn't work", which is exactly how
        // this was first reported.
        const SLOP: f32 = 48.0;
        let (fx, fy) = (
            (x / w.max(1.0)).clamp(0.0, 1.0),
            (y / h.max(1.0)).clamp(0.0, 1.0),
        );
        // A panel's LEFT/RIGHT edge is the layout's when the panel runs the tree's full HEIGHT —
        // then nothing sits above or below it, so that edge is the outside. Likewise a
        // TOP/BOTTOM edge is outer when the panel spans the full WIDTH. (Pairing each side with
        // its own axis instead would be the obvious mistake: a narrow column doesn't span the
        // width, yet its left edge is very much the layout's.)
        let full_w = w >= tw - SLOP;
        let full_h = h >= th - SLOP;
        match self {
            Side::Left => fx <= Self::EDGE_BAND && full_h,
            Side::Right => fx >= 1.0 - Self::EDGE_BAND && full_h,
            Side::Top => fy <= Self::EDGE_BAND && full_w,
            Side::Bottom => fy >= 1.0 - Self::EDGE_BAND && full_w,
        }
    }

    /// The side of `bounds` a point falls in: whichever edge it is nearest, as a fraction of the
    /// panel's own size, so the zones scale with the panel rather than being fixed pixels.
    ///
    /// `x`/`y` are relative to the panel's top-left; `w`/`h` are its size.
    pub fn nearest(x: f32, y: f32, w: f32, h: f32) -> Side {
        let (fx, fy) = (
            (x / w.max(1.0)).clamp(0.0, 1.0),
            (y / h.max(1.0)).clamp(0.0, 1.0),
        );
        // Distance to each edge, in fractions. The smallest wins; ties favour the horizontal
        // axis, which is how side-by-side editors are usually arranged.
        let (l, r, t, b) = (fx, 1.0 - fx, fy, 1.0 - fy);
        let min_h = l.min(r);
        let min_v = t.min(b);
        if min_h <= min_v {
            if l <= r {
                Side::Left
            } else {
                Side::Right
            }
        } else if t <= b {
            Side::Top
        } else {
            Side::Bottom
        }
    }
}

/// Where a hidden panel used to live, so unhiding it restores its place rather than dropping it
/// somewhere arbitrary.
///
/// Without this, showing a panel again split whatever leaf came first — so hiding the Editor and
/// bringing it back landed it beside Git, nowhere near where it had been. A layout control that
/// rearranges your window when you undo it isn't reversible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSlot {
    /// The split the panel belonged to. Reusing the id restores the divider position too, since
    /// fractions are keyed by split id and were never discarded.
    pub split_id: String,
    pub axis: Axis,
    /// Whether the panel was the first (left/top) child.
    pub first: bool,
    /// Every panel in the subtree it shared that split with.
    ///
    /// Restoring wraps the smallest subtree containing exactly these — not merely one of them.
    /// Anchoring on a single panel put `Bottom` (whose sibling is the entire body) back *inside*
    /// the body next to Git, rather than around it.
    pub siblings: Vec<PanelKind>,
}

/// How deep a stored tree may nest before it's rejected.
///
/// A hand-edited or corrupt file must not be able to blow the recursive walker's stack.
pub const MAX_DEPTH: usize = 8;

impl Layout {
    /// A split of `a` and `b`.
    pub fn split(id: &str, axis: Axis, a: Layout, b: Layout) -> Self {
        Layout::Split {
            id: id.to_string(),
            axis,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// The Assistant default: explorer (git over files) | chat | editor, with the bottom strip
    /// beneath.
    ///
    /// The split ids are the ones the app has *always* used, so an upgrading user's saved
    /// fractions apply to this tree with no migration step.
    pub fn assistant_default() -> Self {
        Layout::split(
            crate::splits::id::BODY_BOTTOM,
            Axis::Vertical,
            Layout::split(
                crate::splits::id::EXPLORER_BODY,
                Axis::Horizontal,
                Layout::split(
                    crate::splits::id::EXPLORER_GIT_FILES,
                    Axis::Vertical,
                    Layout::Leaf(PanelKind::Git),
                    Layout::Leaf(PanelKind::Files),
                ),
                Layout::split(
                    crate::splits::id::CHAT_CODE,
                    Axis::Horizontal,
                    Layout::Leaf(PanelKind::Chat),
                    Layout::Leaf(PanelKind::Editor(EditorId::FIRST)),
                ),
            ),
            Layout::Leaf(PanelKind::Bottom),
        )
    }

    /// The Craft default: the same, minus chat — so the editor takes that width.
    pub fn craft_default() -> Self {
        Layout::split(
            crate::splits::id::BODY_BOTTOM,
            Axis::Vertical,
            Layout::split(
                crate::splits::id::EXPLORER_BODY,
                Axis::Horizontal,
                Layout::split(
                    crate::splits::id::EXPLORER_GIT_FILES,
                    Axis::Vertical,
                    Layout::Leaf(PanelKind::Git),
                    Layout::Leaf(PanelKind::Files),
                ),
                Layout::Leaf(PanelKind::Editor(EditorId::FIRST)),
            ),
            Layout::Leaf(PanelKind::Bottom),
        )
    }

    /// The default for a mode.
    pub fn default_for(craft: bool) -> Self {
        if craft {
            Self::craft_default()
        } else {
            Self::assistant_default()
        }
    }

    /// Every panel in the tree, left to right / top to bottom.
    pub fn panels(&self) -> Vec<PanelKind> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PanelKind>) {
        match self {
            Layout::Leaf(k) => out.push(*k),
            Layout::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Whether `kind` is on screen.
    pub fn contains(&self, kind: PanelKind) -> bool {
        self.panels().contains(&kind)
    }

    /// Every editor pane on screen, in tree order (left to right / top to bottom).
    ///
    /// Tree order matters wherever focus is retargeted after a pane closes — it lands somewhere
    /// the user can see, rather than wherever a map happened to iterate.
    pub fn editor_ids(&self) -> Vec<EditorId> {
        self.panels()
            .into_iter()
            .filter_map(|k| match k {
                PanelKind::Editor(id) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// Whether any editor pane is on screen — the invariant [`Self::sanitize`] enforces.
    pub fn has_editor(&self) -> bool {
        self.panels().iter().any(|k| k.is_editor())
    }

    /// How deeply the tree nests.
    pub fn depth(&self) -> usize {
        match self {
            Layout::Leaf(_) => 1,
            Layout::Split { a, b, .. } => 1 + a.depth().max(b.depth()),
        }
    }

    /// Remove every panel failing `keep`, collapsing each split that loses a child onto the
    /// survivor. `None` when nothing is left.
    ///
    /// This is one mechanism serving three needs: hiding a panel from the View menu, dropping
    /// Assistant panels in Craft mode, and discarding a duplicate. Rebalancing rather than
    /// leaving an empty frame is the same instinct as [`crate::splits::SplitStore::get`]'s
    /// fallback — never wedge the UI over bad data.
    pub fn prune(&self, keep: &impl Fn(PanelKind) -> bool) -> Option<Layout> {
        match self {
            Layout::Leaf(k) => keep(*k).then_some(Layout::Leaf(*k)),
            Layout::Split { id, axis, a, b } => match (a.prune(keep), b.prune(keep)) {
                (Some(a), Some(b)) => Some(Layout::split(id, *axis, a, b)),
                // A split with one child is not a split — collapse onto the survivor so its
                // space is reclaimed instead of leaving a gap.
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Drop a panel from the tree.
    pub fn without(&self, kind: PanelKind) -> Option<Layout> {
        self.prune(&|k| k != kind)
    }

    /// Where a panel sat before it was hidden, so unhiding puts it back.
    ///
    /// Records the split it belonged to: that split's id and axis, the sibling it shared with,
    /// and which side it was on. Restoring rebuilds exactly that split around the sibling, which
    /// also brings back the divider position — the fraction is keyed by the split id, so it was
    /// never lost.
    pub fn slot_of(&self, kind: PanelKind) -> Option<PanelSlot> {
        match self {
            Layout::Leaf(_) => None,
            Layout::Split { id, axis, a, b } => {
                // Directly under this split?
                let here = match (&**a, &**b) {
                    (Layout::Leaf(k), _) if *k == kind => Some(true),
                    (_, Layout::Leaf(k)) if *k == kind => Some(false),
                    _ => None,
                };
                if let Some(first) = here {
                    let sibling = if first { b } else { a };
                    return Some(PanelSlot {
                        split_id: id.clone(),
                        axis: *axis,
                        first,
                        siblings: sibling.panels(),
                    });
                }
                a.slot_of(kind).or_else(|| b.slot_of(kind))
            }
        }
    }

    /// Put `kind` back where [`PanelSlot`] says it was.
    ///
    /// Finds the smallest subtree whose panels are exactly the remembered siblings and rebuilds
    /// the original split around it, on the original side and axis. `None` when no such subtree
    /// exists — the siblings have themselves been hidden or rearranged — and the caller falls
    /// back to [`Self::with`] rather than putting the panel somewhere arbitrary.
    pub fn restore(&self, kind: PanelKind, slot: &PanelSlot) -> Option<Layout> {
        if self.contains(kind) || slot.siblings.is_empty() {
            return None;
        }
        self.restore_at(kind, slot)
    }

    fn restore_at(&self, kind: PanelKind, slot: &PanelSlot) -> Option<Layout> {
        // This subtree IS the remembered sibling → wrap it in the original split.
        if self.panels() == slot.siblings {
            let me = Layout::Leaf(kind);
            let (a, b) = if slot.first {
                (me, self.clone())
            } else {
                (self.clone(), me)
            };
            return Some(Layout::split(&slot.split_id, slot.axis, a, b));
        }
        // Otherwise descend — the sibling subtree is smaller than this one.
        match self {
            Layout::Leaf(_) => None,
            Layout::Split { id, axis, a, b } => {
                if let Some(a2) = a.restore_at(kind, slot) {
                    return Some(Layout::split(id, *axis, a2, (**b).clone()));
                }
                b.restore_at(kind, slot)
                    .map(|b2| Layout::split(id, *axis, (**a).clone(), b2))
            }
        }
    }

    /// Move `kind` to the outer edge of the whole layout — a new full-span column or row.
    ///
    /// Distinct from [`Self::move_panel`], which splits ONE panel. Dropping at the window's edge
    /// means "across the full height/width", so this wraps the entire remaining tree rather than
    /// slicing whatever panel happens to sit there. That's the difference between docking beside
    /// the editor and docking down the side of everything.
    ///
    /// `None` when the panel isn't on screen, or when it's the only one (there would be nothing
    /// left to span).
    pub fn move_to_edge(&self, kind: PanelKind, side: Side) -> Option<Layout> {
        let rest = self.without(kind)?;
        if rest.panels().is_empty() {
            return None;
        }
        // Already the full-span child on that side? Then the drop is a no-op, not a rebuild —
        // otherwise repeated drops would pile up redundant splits.
        if let Layout::Split { axis, a, b, .. } = self {
            if *axis == side.axis() {
                let edge = if side.puts_new_first() { a } else { b };
                if **edge == Layout::Leaf(kind) {
                    return None;
                }
            }
        }
        let id = format!("edge:{}:{}", kind.slug(), side.slug());
        let me = Layout::Leaf(kind);
        let (a, b) = if side.puts_new_first() {
            (me, rest)
        } else {
            (rest, me)
        };
        Some(Layout::split(&id, side.axis(), a, b))
    }

    /// Move `kind` so it sits on the `side` of `target`.
    ///
    /// Two steps: prune it from where it was (collapsing the split it leaves behind), then split
    /// the target's leaf and insert it. The new split gets a generated id, so it starts at the
    /// 0.5 default rather than inheriting an unrelated divider's saved position.
    ///
    /// `None` when the move is meaningless or impossible — dropping a panel onto itself, an
    /// unknown target, or a move that would leave no editor.
    pub fn move_panel(&self, kind: PanelKind, target: PanelKind, side: Side) -> Option<Layout> {
        if kind == target || !self.contains(kind) || !self.contains(target) {
            return None;
        }
        let pruned = self.without(kind)?;
        // The target must survive the prune — it will, unless it was inside the moved subtree,
        // which can't happen since leaves hold exactly one panel.
        if !pruned.contains(target) {
            return None;
        }
        let id = format!("{}|{}", kind.slug(), target.slug());
        Some(pruned.insert_beside(kind, target, side, &id))
    }

    fn insert_beside(&self, kind: PanelKind, target: PanelKind, side: Side, id: &str) -> Layout {
        match self {
            Layout::Leaf(k) if *k == target => {
                let me = Layout::Leaf(kind);
                let this = Layout::Leaf(*k);
                let (a, b) = if side.puts_new_first() {
                    (me, this)
                } else {
                    (this, me)
                };
                Layout::split(id, side.axis(), a, b)
            }
            Layout::Leaf(k) => Layout::Leaf(*k),
            Layout::Split {
                id: sid,
                axis,
                a,
                b,
            } => {
                if a.contains(target) {
                    Layout::split(
                        sid,
                        *axis,
                        a.insert_beside(kind, target, side, id),
                        (**b).clone(),
                    )
                } else {
                    Layout::split(
                        sid,
                        *axis,
                        (**a).clone(),
                        b.insert_beside(kind, target, side, id),
                    )
                }
            }
        }
    }

    /// Add `kind` by splitting the first leaf it finds.
    ///
    /// The fallback for when no remembered slot applies (a fresh install, or the panel it used to
    /// sit beside is itself hidden). It lands somewhere visible and the user can drag it.
    pub fn with(&self, kind: PanelKind, id: &str, axis: Axis) -> Layout {
        if self.contains(kind) {
            return self.clone();
        }
        match self {
            Layout::Leaf(k) => Layout::split(id, axis, Layout::Leaf(*k), Layout::Leaf(kind)),
            Layout::Split {
                id: sid,
                axis: sax,
                a,
                b,
            } => Layout::split(sid, *sax, a.with(kind, id, axis), (**b).clone()),
        }
    }

    /// Insert `kind` beside `target`, splitting the leaf that `target` occupies.
    ///
    /// Unlike [`Self::with`], which descends to the first leaf it finds, this places the new
    /// panel *where you asked* — the difference between "split the editor" and "split whatever
    /// happens to be leftmost in the tree". `None` when `target` isn't on screen, or `kind`
    /// already is.
    pub fn insert_at(
        &self,
        kind: PanelKind,
        target: PanelKind,
        side: Side,
        id: &str,
    ) -> Option<Layout> {
        if self.contains(kind) || !self.contains(target) {
            return None;
        }
        Some(self.insert_beside(kind, target, side, id))
    }

    /// Drop duplicate panels, keeping the first of each.
    ///
    /// Two leaves of the same kind would render the same state twice — one tree driven from two
    /// places, one scroll position from two scrollables — which reads as a bug rather than a
    /// feature.
    ///
    /// Editor panes are distinguished by their [`EditorId`], so `Editor(0)` and `Editor(1)` are
    /// two *different* leaves and both survive: that is the whole point of panes. Two leaves
    /// carrying the **same** id are still the duplicate this removes — that really would be one
    /// pane rendered twice, sharing a widget id and a scroll offset.
    ///
    /// No code change was needed for that; the `BTreeSet` already tells the ids apart.
    pub fn dedup(&self) -> Option<Layout> {
        let mut seen = BTreeSet::new();
        self.dedup_inner(&mut seen)
    }

    fn dedup_inner(&self, seen: &mut BTreeSet<PanelKind>) -> Option<Layout> {
        match self {
            Layout::Leaf(k) => seen.insert(*k).then_some(Layout::Leaf(*k)),
            Layout::Split { id, axis, a, b } => {
                // Left first, so "the first of each" means leftmost/topmost.
                let a = a.dedup_inner(seen);
                let b = b.dedup_inner(seen);
                match (a, b) {
                    (Some(a), Some(b)) => Some(Layout::split(id, *axis, a, b)),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    /// Make a loaded tree safe to render, or reject it.
    ///
    /// Returns `None` when the tree is unusable, and the caller falls back to the default. The
    /// rules, all "fall back, never wedge":
    ///   * too deep ⇒ reject (a hand-edited file must not blow the stack)
    ///   * duplicates ⇒ keep the first
    ///   * Assistant panels in Craft mode ⇒ pruned and rebalanced
    ///   * no editor PANE at all ⇒ reject; an IDE with nowhere to open a file is worse than a
    ///     reset layout. Note this counts panes, not "the editor": closing one of several is
    ///     fine, closing the last is not.
    pub fn sanitize(self, craft: bool) -> Option<Layout> {
        if self.depth() > MAX_DEPTH {
            return None;
        }
        let tree = self.dedup()?;
        let tree = if craft {
            tree.prune(&|k| !k.needs_model())?
        } else {
            tree
        };
        tree.has_editor().then_some(tree)
    }

    /// Serialize to JSON. Hand-rolled over `serde_json::Value` to match the house style — no
    /// `derive(Serialize)` exists anywhere in this crate.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Layout::Leaf(k) => serde_json::json!({ "leaf": k.slug() }),
            Layout::Split { id, axis, a, b } => serde_json::json!({
                "split": {
                    "id": id,
                    "axis": axis.slug(),
                    "a": a.to_json(),
                    "b": b.to_json(),
                }
            }),
        }
    }

    /// Parse a tree. `None` on anything malformed — the caller uses the default.
    pub fn parse(v: &serde_json::Value) -> Option<Layout> {
        Self::parse_at(v, 0)
    }

    fn parse_at(v: &serde_json::Value, depth: usize) -> Option<Layout> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(leaf) = v.get("leaf").and_then(|x| x.as_str()) {
            return PanelKind::from_slug(leaf).map(Layout::Leaf);
        }
        let s = v.get("split")?;
        let id = s.get("id")?.as_str()?;
        let axis = Axis::from_slug(s.get("axis")?.as_str()?)?;
        let a = Self::parse_at(s.get("a")?, depth + 1);
        let b = Self::parse_at(s.get("b")?, depth + 1);
        match (a, b) {
            (Some(a), Some(b)) => Some(Layout::split(id, axis, a, b)),
            // One child surviving is still usable — collapse rather than discarding the lot.
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }
}

/// `%APPDATA%\smart-coder\layout.json` — the per-mode trees, beside the other state files.
///
/// `SC_STATE_DIR` redirects it. That exists for tests: `App::default()` reads real machine state,
/// so a test that toggles a panel would otherwise rewrite the developer's actual layout — which
/// it did, once, before this override was added.
fn layout_file() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("SC_STATE_DIR") {
        return std::path::PathBuf::from(dir).join("layout.json");
    }
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("smart-coder").join("layout.json")
}

/// The saved layouts, one per mode.
///
/// Per-mode so toggling back and forth finds each arrangement as it was left — a shared tree
/// would mean entering Craft mode silently rearranged the Assistant one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutStore {
    pub craft: Option<Layout>,
    pub assistant: Option<Layout>,
}

impl LayoutStore {
    /// The stored tree for a mode, or the default when absent/unusable.
    pub fn get(&self, craft: bool) -> Layout {
        let stored = if craft {
            self.craft.clone()
        } else {
            self.assistant.clone()
        };
        stored
            .and_then(|l| l.sanitize(craft))
            .unwrap_or_else(|| Layout::default_for(craft))
    }

    /// Record the tree for a mode (in memory — call [`Self::save`] to persist).
    pub fn set(&mut self, craft: bool, layout: Layout) {
        if craft {
            self.craft = Some(layout);
        } else {
            self.assistant = Some(layout);
        }
    }

    /// Load, or an empty store when the file is missing/unreadable/corrupt.
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(layout_file()) else {
            return Self::default();
        };
        Self::parse(&text)
    }

    /// Parse the file's text. Pure, so the round trip is testable.
    pub fn parse(text: &str) -> Self {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return Self::default();
        };
        Self {
            craft: v.get("craft").and_then(Layout::parse),
            assistant: v.get("assistant").and_then(Layout::parse),
        }
    }

    /// Serialize. Pure.
    pub fn serialize(&self) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert("version".to_string(), serde_json::json!(1));
        if let Some(l) = &self.craft {
            obj.insert("craft".to_string(), l.to_json());
        }
        if let Some(l) = &self.assistant {
            obj.insert("assistant".to_string(), l.to_json());
        }
        serde_json::Value::Object(obj).to_string()
    }

    /// Persist (best-effort — a lost layout is not worth interrupting the user, exactly as
    /// [`crate::splits::SplitStore::save`] treats a lost divider position).
    pub fn save(&self) {
        let path = layout_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.serialize());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_carry_every_panel_a_mode_should_have() {
        let a = Layout::assistant_default();
        for k in [
            PanelKind::Git,
            PanelKind::Files,
            PanelKind::Chat,
            PanelKind::Editor(EditorId::FIRST),
            PanelKind::Bottom,
        ] {
            assert!(a.contains(k), "assistant default missing {}", k.slug());
        }

        // Craft is the same minus chat — the editor takes that width.
        let c = Layout::craft_default();
        assert!(!c.contains(PanelKind::Chat), "no chat without a model");
        assert!(c.contains(PanelKind::Editor(EditorId::FIRST)));
        assert!(c.contains(PanelKind::Files));
        assert!(a.depth() <= MAX_DEPTH && c.depth() <= MAX_DEPTH);
    }

    #[test]
    fn the_default_reuses_the_split_ids_the_app_already_persisted() {
        // THE migration story. An upgrading user's `splits.json` holds `chat|code` and
        // `explorer:git|files`; reusing those ids means their divider positions load into the
        // new tree with no migration code at all. Renaming one silently resets everyone's
        // layout, which is exactly what this test exists to prevent.
        let json = Layout::assistant_default().to_json().to_string();
        for id in [
            crate::splits::id::CHAT_CODE,
            crate::splits::id::EXPLORER_GIT_FILES,
        ] {
            assert!(
                json.contains(id),
                "default tree must use the legacy id {id}"
            );
        }
    }

    #[test]
    fn hiding_a_panel_collapses_its_split_onto_the_survivor() {
        // A split that loses a child is not a split — the survivor must reclaim the space
        // rather than leaving an empty frame.
        let l = Layout::assistant_default();
        let without_chat = l.without(PanelKind::Chat).expect("still usable");
        assert!(!without_chat.contains(PanelKind::Chat));
        assert!(
            without_chat.contains(PanelKind::Editor(EditorId::FIRST)),
            "editor survives"
        );
        // The `chat|code` split is gone entirely — the editor took its place rather than sitting
        // beside an empty frame. (Overall depth is unchanged, because the explorer's git|files
        // split is what sets it.)
        assert!(
            !without_chat
                .to_json()
                .to_string()
                .contains(crate::splits::id::CHAT_CODE),
            "the emptied split collapsed away: {without_chat:?}"
        );
        // Dropping chat from the Assistant default yields exactly the Craft default — the two
        // are the same layout, which is the claim spec 21 makes about the two modes.
        assert_eq!(without_chat, Layout::craft_default());
    }

    #[test]
    fn a_craft_layout_that_still_lists_chat_is_pruned_not_rejected() {
        // Carried across a mode switch, or hand-edited. Rendering an Assistant panel in a mode
        // that contacts no model would break the mode's promise; refusing to start would be
        // worse. Prune and rebalance.
        let sane = Layout::assistant_default()
            .sanitize(true)
            .expect("still usable");
        assert!(!sane.contains(PanelKind::Chat));
        assert!(sane.contains(PanelKind::Editor(EditorId::FIRST)));
    }

    #[test]
    fn a_layout_with_no_editor_is_rejected_for_the_default() {
        // An IDE with no editor is worse than a reset layout.
        let no_editor = Layout::split(
            "x",
            Axis::Horizontal,
            Layout::Leaf(PanelKind::Files),
            Layout::Leaf(PanelKind::Git),
        );
        assert_eq!(no_editor.clone().sanitize(false), None);

        let store = LayoutStore {
            assistant: Some(no_editor),
            craft: None,
        };
        assert_eq!(
            store.get(false),
            Layout::assistant_default(),
            "falls back rather than rendering something unusable"
        );
    }

    #[test]
    fn editor_zero_is_still_spelled_editor() {
        // COMPATIBILITY. Every layout.json written before panes had ids says `"editor"`, and the
        // split ids keyed into splits.json are built from these slugs. Spelling the first pane
        // `editor:0` would parse fine but silently reset every existing user's saved dividers,
        // because `chat|editor:0` is not the key `chat|editor` they were stored under.
        assert_eq!(PanelKind::Editor(EditorId::FIRST).slug(), "editor");
        assert_eq!(PanelKind::Editor(EditorId(1)).slug(), "editor:1");

        // Both spellings parse, and each round-trips to itself.
        assert_eq!(
            PanelKind::from_slug("editor"),
            Some(PanelKind::Editor(EditorId::FIRST)),
            "a layout written before panes existed still loads"
        );
        for k in [
            PanelKind::Editor(EditorId::FIRST),
            PanelKind::Editor(EditorId(7)),
        ] {
            assert_eq!(PanelKind::from_slug(&k.slug()), Some(k));
        }
        // Garbage after the prefix is rejected rather than defaulting to pane 0.
        assert_eq!(PanelKind::from_slug("editor:x"), None);
        assert_eq!(PanelKind::from_slug("editor:"), None);
    }

    #[test]
    fn a_single_pane_layout_generates_the_split_ids_it_always_did() {
        // THE test that protects splits.json. These exact id strings are keys into the store of
        // saved divider positions; if this ever fails, upgrading silently resets everyone's
        // layout proportions with no error to notice.
        let one = Layout::Leaf(PanelKind::Editor(EditorId::FIRST));

        let docked = one.move_to_edge(PanelKind::Editor(EditorId::FIRST), Side::Right);
        assert_eq!(docked, None, "already the only panel — nothing to span");

        let with_chat = Layout::split(
            crate::splits::id::CHAT_CODE,
            Axis::Horizontal,
            Layout::Leaf(PanelKind::Chat),
            Layout::Leaf(PanelKind::Editor(EditorId::FIRST)),
        );
        let moved = with_chat
            .move_panel(
                PanelKind::Chat,
                PanelKind::Editor(EditorId::FIRST),
                Side::Right,
            )
            .expect("valid move");
        assert!(
            moved.to_json().to_string().contains("chat|editor"),
            "the generated split id must still be `chat|editor`: {moved:?}"
        );

        // And the default tree still serialises with the legacy spelling throughout.
        let json = Layout::assistant_default().to_json().to_string();
        assert!(json.contains(r#""leaf":"editor""#), "{json}");
        assert!(
            !json.contains("editor:"),
            "no id suffix on a single pane: {json}"
        );
    }

    #[test]
    fn two_leaves_of_the_same_panel_are_reduced_to_the_first() {
        // Every panel but the editor renders ONE piece of app state, so a second leaf would be
        // the same view twice — one tree driven from two places.
        let dup = Layout::split(
            "x",
            Axis::Horizontal,
            Layout::Leaf(PanelKind::Files),
            Layout::Leaf(PanelKind::Files),
        );
        assert_eq!(dup.dedup(), Some(Layout::Leaf(PanelKind::Files)));

        // Two editor leaves carrying the SAME id are the same bug: one pane, one set of tabs,
        // one widget id, rendered twice.
        let same_pane = Layout::split(
            "x",
            Axis::Horizontal,
            Layout::Leaf(PanelKind::Editor(EditorId::FIRST)),
            Layout::Leaf(PanelKind::Editor(EditorId::FIRST)),
        );
        let fixed = same_pane.sanitize(false).expect("usable after dedup");
        assert_eq!(fixed, Layout::Leaf(PanelKind::Editor(EditorId::FIRST)));
    }

    #[test]
    fn two_editor_panes_both_survive() {
        // The inverse of the rule above, and the whole point of panes: distinct ids are distinct
        // leaves, each with its own tabs and scroll, so both stay. This is the assertion that
        // changed when editor panes gained identity — the old test asserted they collapsed.
        let two = Layout::split(
            "editor|editor",
            Axis::Horizontal,
            Layout::Leaf(PanelKind::Editor(EditorId::FIRST)),
            Layout::Leaf(PanelKind::Editor(EditorId(1))),
        );
        let kept = two.clone().sanitize(false).expect("two panes are valid");
        assert_eq!(kept, two, "neither pane was pruned");
        assert_eq!(
            kept.editor_ids(),
            vec![EditorId::FIRST, EditorId(1)],
            "and they come back in tree order"
        );
    }

    #[test]
    fn a_pathologically_deep_tree_is_rejected_before_it_can_recurse() {
        // A hand-edited or corrupt file must not blow the walker's stack.
        let mut l = Layout::Leaf(PanelKind::Editor(EditorId::FIRST));
        for i in 0..(MAX_DEPTH + 3) {
            l = Layout::split(
                &format!("d{i}"),
                Axis::Horizontal,
                l,
                Layout::Leaf(PanelKind::Files),
            );
        }
        assert!(l.depth() > MAX_DEPTH);
        assert_eq!(l.sanitize(false), None, "rejected, so the default is used");
    }

    #[test]
    fn a_layout_round_trips_through_json() {
        let store = LayoutStore {
            craft: Some(Layout::craft_default()),
            assistant: Some(Layout::assistant_default()),
        };
        let back = LayoutStore::parse(&store.serialize());
        assert_eq!(back, store);
        // And each mode keeps its own arrangement — a shared tree would mean entering Craft
        // silently rearranged the Assistant one.
        assert_ne!(back.get(true), back.get(false));
    }

    #[test]
    fn a_corrupt_layout_file_falls_back_to_the_defaults() {
        for junk in [
            "",
            "not json",
            "[1,2,3]",
            r#"{"assistant":{"leaf":"nope"}}"#,
        ] {
            let store = LayoutStore::parse(junk);
            assert_eq!(
                store.get(false),
                Layout::assistant_default(),
                "junk: {junk:?}"
            );
            assert_eq!(store.get(true), Layout::craft_default());
        }
    }

    #[test]
    fn dragging_a_panel_onto_another_puts_it_on_the_chosen_side() {
        let before = Layout::assistant_default();

        // Drop Git to the RIGHT of the editor: it leaves the explorer column (collapsing that
        // split onto Files) and becomes the editor's right-hand neighbour.
        let after = before
            .move_panel(
                PanelKind::Git,
                PanelKind::Editor(EditorId::FIRST),
                Side::Right,
            )
            .expect("a valid move");

        assert!(after.contains(PanelKind::Git), "still on screen");
        assert!(
            after.contains(PanelKind::Files),
            "and so is its old sibling"
        );
        // The panels are the same set — a move must never lose or duplicate one.
        let mut a = before.panels();
        let mut b = after.panels();
        a.sort();
        b.sort();
        assert_eq!(a, b, "a move changes arrangement, not membership");

        // Git now sits immediately after Editor in reading order.
        let order = after.panels();
        let ie = order
            .iter()
            .position(|k| *k == PanelKind::Editor(EditorId::FIRST))
            .unwrap();
        assert_eq!(
            order.get(ie + 1),
            Some(&PanelKind::Git),
            "dropped to its right"
        );
    }

    #[test]
    fn dropping_at_the_window_edge_spans_the_whole_layout() {
        // The distinction that matters: an edge drop docks down the side of EVERYTHING, so the
        // panel becomes a full-height column — not a slice of whichever panel happened to be
        // under the cursor.
        let before = Layout::assistant_default();
        let after = before
            .move_to_edge(PanelKind::Git, Side::Right)
            .expect("valid edge drop");

        match &after {
            Layout::Split { axis, a, b, .. } => {
                assert_eq!(*axis, Axis::Horizontal, "a column");
                assert_eq!(**b, Layout::Leaf(PanelKind::Git), "Git is the right column");
                // Everything else is the other child, so Git spans the full height beside it.
                let mut rest = a.panels();
                rest.sort();
                let mut expected = before.without(PanelKind::Git).unwrap().panels();
                expected.sort();
                assert_eq!(rest, expected, "the rest of the layout is intact");
            }
            other => panic!("expected a root split, got {other:?}"),
        }

        // Top puts it first, as a full-width row.
        let top = before.move_to_edge(PanelKind::Bottom, Side::Top).unwrap();
        match &top {
            Layout::Split { axis, a, .. } => {
                assert_eq!(*axis, Axis::Vertical, "a row");
                assert_eq!(**a, Layout::Leaf(PanelKind::Bottom), "and it's on top");
            }
            other => panic!("expected a root split, got {other:?}"),
        }
    }

    #[test]
    fn re_dropping_a_panel_on_the_edge_it_already_occupies_is_a_no_op() {
        // Otherwise every repeat drop would wrap another redundant split around the tree, and
        // the layout would accumulate junk nesting until it hit MAX_DEPTH.
        let docked = Layout::assistant_default()
            .move_to_edge(PanelKind::Git, Side::Right)
            .unwrap();
        assert_eq!(docked.move_to_edge(PanelKind::Git, Side::Right), None);
        // But moving it to a DIFFERENT edge still works.
        assert!(docked.move_to_edge(PanelKind::Git, Side::Top).is_some());
    }

    #[test]
    fn an_edge_drop_needs_the_panel_to_span_the_layout() {
        // The tree does NOT fill the window — a menu bar sits above it, and sometimes a gate bar
        // below. Measuring against the WINDOW is why bottom docking silently never fired: the
        // bottom panel's edge could never reach `window_h`. The rule is instead about SPANS,
        // which needs no origin: a panel at the tree's edge is one that reaches across it.
        const MENU_BAR: f32 = 34.0;
        let (tw, th) = (1000.0, 800.0 - MENU_BAR);

        // A full-HEIGHT column (e.g. the explorer). Its left and right edges run the tree's full
        // height, so a drop in either band docks a full-height column beside everything.
        let (w, h) = (200.0, th);
        assert!(Side::Left.is_outer(5.0, 400.0, w, h, tw, th));
        assert!(Side::Right.is_outer(195.0, 400.0, w, h, tw, th));
        // Its TOP and BOTTOM are interior — the column doesn't span the tree's width, so those
        // edges border other panels rather than the layout.
        assert!(!Side::Top.is_outer(100.0, 5.0, w, h, tw, th));
        assert!(!Side::Bottom.is_outer(100.0, h - 5.0, w, h, tw, th));

        // THE REPORTED BUG: a full-WIDTH panel (the bottom strip). Its lower edge is nowhere
        // near the window's height, but it spans the tree — so a drop there docks full-width.
        let (w, h) = (tw, 200.0);
        assert!(
            Side::Bottom.is_outer(500.0, 195.0, w, h, tw, th),
            "a full-width panel's bottom band must dock across the layout"
        );
        assert!(
            Side::Top.is_outer(500.0, 5.0, w, h, tw, th),
            "and so must its top band"
        );

        // A panel spanning neither axis: nothing about it is outer.
        let (w, h) = (200.0, 200.0);
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            assert!(
                !side.is_outer(5.0, 5.0, w, h, tw, th),
                "{side:?} on an interior panel must split, not span"
            );
        }

        // Well inside a spanning panel → still not an edge drop, so ordinary splitting wins.
        assert!(!Side::Right.is_outer(500.0, 400.0, tw, th, tw, th));
    }

    #[test]
    fn a_panels_chrome_does_not_stop_it_counting_as_full_span() {
        // The drop zones are measured over a panel's BODY, which is shorter than the panel by
        // its drag header and padding. A full-height column therefore reports a body several
        // pixels short of the tree — and if the tolerance can't absorb that, side docking
        // silently stops working. Same class of bug as the bottom-edge one.
        const HEADER: f32 = 21.0; // text + padding
        const CHROME: f32 = 16.0; // padding top and bottom
        let (tw, th) = (1000.0, 766.0);
        let (w, h) = (200.0, th - HEADER - CHROME);

        assert!(
            Side::Left.is_outer(5.0, 300.0, w, h, tw, th),
            "a full-height column must still span once its header is accounted for"
        );
        assert!(Side::Right.is_outer(w - 5.0, 300.0, w, h, tw, th));

        // But a genuinely half-height panel must NOT — the tolerance is for chrome, not for
        // turning every panel into an edge.
        let half = th * 0.5;
        assert!(!Side::Left.is_outer(5.0, 100.0, w, half, tw, th));
    }

    #[test]
    fn dropping_on_the_left_or_top_puts_the_panel_first() {
        let before = Layout::assistant_default();
        let after = before
            .move_panel(
                PanelKind::Bottom,
                PanelKind::Editor(EditorId::FIRST),
                Side::Left,
            )
            .expect("valid");
        let order = after.panels();
        let ie = order
            .iter()
            .position(|k| *k == PanelKind::Editor(EditorId::FIRST))
            .unwrap();
        assert_eq!(
            order.get(ie.wrapping_sub(1)),
            Some(&PanelKind::Bottom),
            "dropped to its left, so it comes first"
        );
    }

    #[test]
    fn a_meaningless_move_is_refused_rather_than_mangling_the_tree() {
        let l = Layout::assistant_default();
        // Onto itself.
        assert_eq!(
            l.move_panel(PanelKind::Git, PanelKind::Git, Side::Left),
            None
        );
        // A panel that isn't on screen can't be moved or targeted.
        let no_git = l.without(PanelKind::Git).unwrap();
        assert_eq!(
            no_git.move_panel(
                PanelKind::Git,
                PanelKind::Editor(EditorId::FIRST),
                Side::Left
            ),
            None
        );
        assert_eq!(
            no_git.move_panel(
                PanelKind::Editor(EditorId::FIRST),
                PanelKind::Git,
                Side::Left
            ),
            None
        );
    }

    #[test]
    fn the_drop_side_is_whichever_edge_the_cursor_is_nearest() {
        // A 100x100 panel. Near an edge → that side; the zones scale with the panel rather than
        // being fixed pixels, so a narrow panel is still droppable.
        assert_eq!(Side::nearest(5.0, 50.0, 100.0, 100.0), Side::Left);
        assert_eq!(Side::nearest(95.0, 50.0, 100.0, 100.0), Side::Right);
        assert_eq!(Side::nearest(50.0, 5.0, 100.0, 100.0), Side::Top);
        assert_eq!(Side::nearest(50.0, 95.0, 100.0, 100.0), Side::Bottom);
        // Sides compose the right axis and ordering.
        assert_eq!(Side::Left.axis(), Axis::Horizontal);
        assert_eq!(Side::Top.axis(), Axis::Vertical);
        assert!(Side::Left.puts_new_first() && Side::Top.puts_new_first());
        assert!(!Side::Right.puts_new_first() && !Side::Bottom.puts_new_first());
        // Degenerate sizes must not divide by zero.
        let _ = Side::nearest(0.0, 0.0, 0.0, 0.0);
    }

    #[test]
    fn a_moved_panel_survives_a_round_trip_through_json() {
        // A rearranged layout has generated split ids; they must persist like any other.
        let moved = Layout::assistant_default()
            .move_panel(
                PanelKind::Git,
                PanelKind::Editor(EditorId::FIRST),
                Side::Bottom,
            )
            .unwrap();
        let store = LayoutStore {
            assistant: Some(moved.clone()),
            craft: None,
        };
        assert_eq!(LayoutStore::parse(&store.serialize()).get(false), moved);
    }

    #[test]
    fn a_hidden_panel_can_be_brought_back() {
        let l = Layout::craft_default();
        let hidden = l.without(PanelKind::Git).expect("usable");
        assert!(!hidden.contains(PanelKind::Git));

        let restored = hidden.with(PanelKind::Git, "restored", Axis::Horizontal);
        assert!(restored.contains(PanelKind::Git), "back on screen");

        // Adding one that's already present is a no-op, not a duplicate.
        assert_eq!(
            restored.with(PanelKind::Git, "again", Axis::Horizontal),
            restored
        );
    }

    #[test]
    fn hiding_a_panel_and_showing_it_again_restores_the_exact_layout() {
        // THE reported bug: hide a panel, tick it back on, and it reappeared beside Git —
        // wherever the first leaf happened to be — instead of where it came from. A layout
        // control that rearranges your window when you undo it is not reversible.
        for kind in [
            PanelKind::Chat,
            PanelKind::Git,
            PanelKind::Files,
            PanelKind::Bottom,
        ] {
            let before = Layout::assistant_default();
            let slot = before.slot_of(kind).expect("panel is in the default tree");
            let hidden = before.without(kind).expect("still usable without it");
            assert!(!hidden.contains(kind));

            let after = hidden.restore(kind, &slot).expect("anchor still on screen");
            assert_eq!(
                after,
                before,
                "{} did not return to where it was",
                kind.slug()
            );
        }
    }

    #[test]
    fn restoring_falls_back_when_the_old_neighbour_is_also_hidden() {
        // Hide Chat, then hide the Editor it sat beside. Chat's remembered slot anchors on a
        // panel that is no longer on screen, so `restore` declines and the caller uses `with`
        // rather than producing a tree with a dangling reference.
        let before = Layout::assistant_default();
        let chat_slot = before.slot_of(PanelKind::Chat).unwrap();
        assert_eq!(
            chat_slot.siblings,
            vec![PanelKind::Editor(EditorId::FIRST)],
            "sat beside the editor"
        );

        let hidden = before
            .without(PanelKind::Chat)
            .and_then(|l| l.without(PanelKind::Editor(EditorId::FIRST)))
            .expect("usable");

        assert_eq!(
            hidden.restore(PanelKind::Chat, &chat_slot),
            None,
            "declines rather than guessing"
        );
    }

    #[test]
    fn a_restored_panel_keeps_its_old_divider_position() {
        // The fraction lives in SplitStore keyed by split id, so reusing the id on restore means
        // the divider comes back where the user had dragged it — not reset to the default.
        let before = Layout::assistant_default();
        let slot = before.slot_of(PanelKind::Chat).unwrap();
        assert_eq!(
            slot.split_id,
            crate::splits::id::CHAT_CODE,
            "restores into the SAME split id, so its saved fraction still applies"
        );
    }
}
