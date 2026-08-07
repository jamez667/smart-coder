//! Rendering the panel tree.
//!
//! A recursive walker over [`sc_win::layout::Layout`]: every node becomes nested rows/columns of
//! `FillPortion`, and every split gets a draggable divider keyed by its node id.
//!
//! Two things make this simpler than the hardcoded layout it replaces:
//!
//! * **Sizing lives here, not in the panels.** Each `view_panel` returns `Fill × Fill` and the
//!   walker applies the portions, so panels have no idea where they are. That is what lets the
//!   same panel appear anywhere in the tree.
//! * **Drags are delta-mapped, and the extent comes from [`iced::widget::responsive`].** The
//!   closure runs inside `layout()`, so the available size is exact and synchronous — no `Task`
//!   round-trip, no frame lag. Because a delta only needs the *extent*, never the origin, the
//!   old `0.20 * window_w` literal and `explorer_region_h`'s stack of guessed chrome constants
//!   are gone. Spec 21.

use iced::widget::{column, container, responsive, row, text, Space};
use iced::{Element, Fill, Length};

use sc_win::layout::{Axis, Layout, PanelKind, Side};

use super::{
    card_style, dock_band_active_style, dock_band_style, drag_ghost_style,
    drop_zone_active_span_style, drop_zone_active_style, drop_zone_idle_style,
    drop_zone_span_style, h_divider_draggable, panel_header_dragging_style, panel_header_style,
    v_divider_draggable, App, BottomTab, Message, FG, FG_MUTED, GAP, PAD,
};

/// What is currently being carried by a header/tab drag.
///
/// ONE field on [`App`] rather than two parallel `Option`s, because the drop machinery — the
/// window dock frame, the per-panel zones, the global release catch, the ghost — is identical for
/// both and must never see a state where both are set. Only the *resolution* differs: a panel
/// moves a leaf in the tree, a tab moves a path between panes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DragSubject {
    /// A whole panel, picked up by its header.
    Panel(PanelKind),
    /// A single editor tab, picked up from its strip.
    ///
    /// Carries where it came from, so a drop back onto its own pane is a no-op rather than a
    /// remove-then-add that would lose the buffer.
    Tab {
        from: sc_win::layout::EditorId,
        path: String,
        /// Whether the cursor has yet moved far enough to count as a drag. Until it has, this is
        /// still a *click* — which is why selection happens on release, not on press. A tab has
        /// two jobs (select, close) where a panel header has one, so without a threshold every
        /// click would be a one-pixel drag and the strip would flicker.
        armed: bool,
        /// Cursor position at the press, to measure the threshold against.
        origin: iced::Point,
    },
}

impl DragSubject {
    /// How far the cursor must travel before a tab press becomes a drag.
    pub(crate) const THRESHOLD: f32 = 4.0;

    /// The panel being dragged, if this is a panel drag. Lets the shared drop machinery keep
    /// asking the question it always asked.
    pub(crate) fn panel(&self) -> Option<PanelKind> {
        match self {
            DragSubject::Panel(k) => Some(*k),
            DragSubject::Tab { .. } => None,
        }
    }

    /// Whether this drag should currently paint drop targets. A tab that hasn't crossed the
    /// threshold is still a click, so the whole UI must stay quiet.
    pub(crate) fn is_active(&self) -> bool {
        match self {
            DragSubject::Panel(_) => true,
            DragSubject::Tab { armed, .. } => *armed,
        }
    }
}

/// A divider drag in progress.
#[derive(Debug, Clone)]
pub(crate) struct Drag {
    /// Which split is being dragged — its [`sc_win::splits`] id.
    pub(crate) id: String,
    pub(crate) axis: Axis,
    /// The region's size along the drag axis, from `responsive`. Exact, so the divider tracks
    /// the cursor 1:1 instead of lagging or racing it.
    pub(crate) extent: f32,
    /// Cursor position along the axis when the drag started.
    pub(crate) origin: f32,
    /// The fraction at that moment.
    pub(crate) frac0: f32,
}

impl App {
    /// The PANEL currently being dragged, if the drag is a panel drag at all.
    ///
    /// Most of the drop machinery predates tab drags and only ever asked this question; keeping
    /// the accessor means those sites read the same as before rather than each growing a match.
    pub(crate) fn dragged_panel(&self) -> Option<PanelKind> {
        self.drag.as_ref().and_then(|d| d.panel())
    }

    /// Whether a drag is in flight AND has committed to being one — a tab press that hasn't
    /// crossed the threshold is still a click, and must not light up the whole UI.
    pub(crate) fn dragging(&self) -> bool {
        self.drag.as_ref().is_some_and(|d| d.is_active())
    }

    /// Render the whole panel tree, with the window-edge drop frame over it while dragging.
    ///
    /// The tree's on-screen rect is reported by `responsive` — measured, never assumed to be the
    /// window. The tree sits below the menu bar, so its bottom edge is nowhere near `window_h`;
    /// testing against the window is what stopped bottom-edge docking from ever firing.
    pub(crate) fn view_layout(&self) -> Element<'_, Message> {
        responsive(move |size| {
            let tree = self.view_node(&self.layout, size);
            if !self.dragging() {
                return tree;
            }
            // Mid-drag the tree is INSET by the band width, so the dock frame occupies real
            // space instead of floating over the panels. Seeing the layout physically make room
            // is what makes the drop obvious — the bands are a place, not an overlay.
            let inset = container(tree).width(Fill).height(Fill).padding(DOCK_BAND);
            iced::widget::stack![
                inset,
                self.view_window_dock_frame(),
                // The ghost rides on top of everything so it's never clipped by a panel.
                self.view_drag_ghost(),
            ]
            .into()
        })
        .into()
    }

    /// A ghost of the panel being dragged, following the cursor.
    ///
    /// Its shape, header and a short description of what it holds — so you can see *what* you're
    /// carrying, not merely that something is in flight. Pinned absolutely, anchored as if held
    /// by the header where the grab began.
    ///
    /// Deliberately a SHAPE-AND-SUMMARY rather than a picture of the panel. A cropped screenshot
    /// was tried and looked worse: scaled into a small box it reads as a smeared thumbnail, and
    /// it can't be re-rendered per frame anyway (re-laying-out a code editor at mouse-move rate
    /// would make the drag stutter). The card says what it is; that's the useful part.
    fn view_drag_ghost(&self) -> Element<'_, Message> {
        // A dragged TAB is one file, so its ghost is a small chip rather than a panel-sized card —
        // the size says which of the two gestures is in flight before you read either label.
        let kind = match &self.drag {
            Some(DragSubject::Panel(k)) => *k,
            Some(DragSubject::Tab { path, .. }) => {
                let base = path.rsplit(['/', '\\']).next().unwrap_or(path.as_str());
                let x = (self.cursor_pos.x + 8.0).max(0.0);
                let y = (self.cursor_pos.y + 8.0).max(0.0);
                let chip = container(text(base.to_string()).size(11).color(FG))
                    .padding([4, 8])
                    .style(drag_ghost_style);
                return iced::widget::pin(chip).x(x).y(y).into();
            }
            None => return Space::new().into(),
        };
        const W: f32 = 240.0;
        const H: f32 = 150.0;
        // Anchored near the cursor's top-left, as if carried by its header.
        let x = (self.cursor_pos.x - W * 0.25).max(0.0);
        let y = (self.cursor_pos.y - 10.0).max(0.0);

        // A cheap, honest summary of the panel's contents.
        let summary: String = match kind {
            PanelKind::Editor(_) => match self.active_tab() {
                Some(t) => t.path.clone(),
                None => "no file open".to_string(),
            },
            PanelKind::Files => match &self.picked_workspace {
                Some(p) => p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("project")
                    .to_string(),
                None => "no project".to_string(),
            },
            PanelKind::Git => match &self.branch {
                Some(b) => format!("⎇ {b}  ·  {} changed", self.file_status.len()),
                None => "not a git repo".to_string(),
            },
            PanelKind::Bottom => match self.bottom_tab {
                BottomTab::Problems => "Problems".to_string(),
                BottomTab::Terminal => "Terminal".to_string(),
                BottomTab::Verification => "Verification".to_string(),
                BottomTab::Build => "Build".to_string(),
            },
            PanelKind::Chat => format!("{} messages", self.chat_turns.len()),
        };

        let ghost = container(
            column![
                container(
                    row![
                        text(kind.label()).size(11).color(FG),
                        Space::new().width(Fill),
                        text("⠿").size(11).color(FG_MUTED),
                    ]
                    .align_y(iced::Alignment::Center),
                )
                .width(Fill)
                .padding([3, 8])
                .style(panel_header_dragging_style),
                container(text(summary).size(11).color(FG_MUTED))
                    .width(Fill)
                    .height(Fill)
                    .padding(PAD),
            ]
            .width(Fill)
            .height(Fill),
        )
        .width(Length::Fixed(W))
        .height(Length::Fixed(H))
        .style(drag_ghost_style);

        iced::widget::pin(ghost).x(x).y(y).into()
    }

    /// The window-edge dock frame: four bands hugging the outside of the whole layout.
    ///
    /// Distinct from the per-panel zones in [`Self::view_leaf`]. Those split whichever panel you
    /// point at; these dock a full-width row or full-height column across everything, regardless
    /// of what happens to sit under the cursor. Having them on the window frame is what makes
    /// "throw it at the edge" work without aiming.
    fn view_window_dock_frame(&self) -> Element<'_, Message> {
        const BAND: f32 = DOCK_BAND;

        // `w`/`h` size the band's inner container — `mouse_area` itself takes no size, it just
        // wraps whatever it's given.
        let band = |side: Side, w: Length, h: Length| {
            let active = self.dock_side == Some(side);
            iced::widget::mouse_area(
                container(
                    // The label makes the outcome explicit — you can read "full width" before
                    // committing rather than inferring it from a highlight.
                    text(match side {
                        Side::Left | Side::Right => "dock full height",
                        Side::Top | Side::Bottom => "dock full width",
                    })
                    .size(10)
                    .color(if active { FG } else { FG_MUTED }),
                )
                .center_x(Fill)
                .center_y(Fill)
                .width(w)
                .height(h)
                .style(if active {
                    dock_band_active_style
                } else {
                    dock_band_style
                }),
            )
            .on_enter(Message::DockHover(Some(side)))
            .on_exit(Message::DockHover(None))
            .on_release(Message::PanelDrop)
        };

        // A frame: full-width top and bottom, with left/right filling the space between, so the
        // corners belong to the horizontal bands and every edge is reachable.
        column![
            band(Side::Top, Fill, Length::Fixed(BAND)),
            row![
                band(Side::Left, Length::Fixed(BAND), Fill),
                // The middle is transparent and non-interactive, so the per-panel zones and the
                // panels themselves still receive their own events.
                Space::new().width(Fill).height(Fill),
                band(Side::Right, Length::Fixed(BAND), Fill),
            ]
            .height(Fill),
            band(Side::Bottom, Fill, Length::Fixed(BAND)),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }

    /// One node: a panel, or a split of two.
    ///
    /// `tree` is the whole layout's size, threaded down so each panel's drop sensor can report
    /// it — that's what lets `update` tell an outer edge from an interior one.
    fn view_node<'a>(&'a self, node: &'a Layout, tree: iced::Size) -> Element<'a, Message> {
        match node {
            Layout::Leaf(kind) => self.view_leaf(*kind, tree),
            Layout::Split { id, axis, a, b } => {
                let frac = self.splits.get(id, default_frac(id));
                // 1000 portions so a 0.001 drag still moves a pixel on a wide window.
                let pa = (frac * 1000.0).round().clamp(1.0, 999.0) as u16;
                let pb = 1000u16.saturating_sub(pa).max(1);

                let (split_id, axis) = (id.clone(), *axis);
                // `responsive` hands us the region's true size DURING layout — the whole reason
                // the guessed-geometry constants could be deleted. The children are built INSIDE
                // the closure because iced Elements aren't `Clone` and the closure may run more
                // than once; they're cheap views over borrowed state.
                responsive(move |size| {
                    let extent = match axis {
                        Axis::Horizontal => size.width,
                        Axis::Vertical => size.height,
                    };
                    let grab = Message::SplitGrab {
                        id: split_id.clone(),
                        axis,
                        extent,
                    };
                    let first = self.view_node(a, tree);
                    let second = self.view_node(b, tree);
                    match axis {
                        Axis::Horizontal => row![
                            container(first).width(Length::FillPortion(pa)),
                            v_divider_draggable(grab),
                            container(second).width(Length::FillPortion(pb)),
                        ]
                        .spacing(GAP)
                        .height(Fill)
                        .into(),
                        Axis::Vertical => column![
                            container(first).height(Length::FillPortion(pa)),
                            h_divider_draggable(grab),
                            container(second).height(Length::FillPortion(pb)),
                        ]
                        .spacing(GAP)
                        .width(Fill)
                        .into(),
                    }
                })
                .into()
            }
        }
    }

    /// One panel: its drag header, its content, and any drop-edge highlight.
    fn view_leaf(&self, kind: PanelKind, tree: iced::Size) -> Element<'_, Message> {
        let dragging = self.dragged_panel() == Some(kind);
        // The header is the GRAB HANDLE — a shade lighter than the body so it reads as chrome
        // you can pick up, not content. Pressing it starts the drag; the release that ends it is
        // caught globally, so letting go outside the window can't strand a panel mid-flight.
        let header = iced::widget::mouse_area(
            container(
                row![
                    text(kind.label()).size(11),
                    Space::new().width(Fill),
                    // A grip glyph, so the strip reads as draggable rather than decorative.
                    text("⠿").size(11),
                ]
                .align_y(iced::Alignment::Center),
            )
            .width(Fill)
            // Horizontal padding matches the body's, so the title sits flush over the content
            // beneath it rather than being indented differently.
            .padding([2, PANEL_PAD + 2])
            .style(if dragging {
                panel_header_dragging_style
            } else {
                panel_header_style
            }),
        )
        .on_press(Message::PanelGrab(kind))
        .interaction(iced::mouse::Interaction::Grab);

        // Tight inner padding: with GAP at 0 the panels butt together, so each panel's own
        // padding is the ONLY thing between adjacent contents — and it doubles up at every
        // seam. Small enough that text doesn't touch the edge, small enough that two panels
        // side by side read as one surface split by a line rather than two floating cards.
        //
        // The EDITOR is exempt and sits flush: its line-number gutter is meant to start at the
        // panel's edge, and any padding shows as a stripe of background to the left of the
        // numbers. It draws its own background, so it needs no inset to look contained.
        let pad = if kind.is_editor() { 0 } else { PANEL_PAD };
        let mut body: Element<'_, Message> = container(self.view_panel(kind))
            .width(Fill)
            .height(Fill)
            .padding(pad)
            .into();

        // Clicking anywhere in an editor pane focuses it. Focus decides where Ctrl+S saves and
        // where the file tree opens files, so it must follow the pointer rather than being a
        // hidden mode — and `mouse_area` here does not swallow the click, so the editor beneath
        // still receives it.
        if let PanelKind::Editor(id) = kind {
            if self.panes.focused_id() != id {
                body = iced::widget::mouse_area(body)
                    .on_press(Message::FocusPane(id))
                    .into();
            }
        }

        // While something is being dragged, every OTHER panel becomes a live drop target and
        // shows ALL FOUR of its zones at once. Revealing them only on hover meant you had to
        // already know where to aim; showing the whole set makes the available drops legible the
        // moment you pick a panel up, and the hovered one is simply the bright one.
        //
        // The overlay wraps the BODY, not the whole leaf. Stacking it over `header + body` made
        // the bottom band render below the visible area by the header's height — which is why
        // the bottom zone appeared to be missing entirely.
        // A dragged TAB can only land in an editor, so only editor panes light up for one —
        // offering the git panel as a target would promise a drop that has nowhere to go. A
        // dragged PANEL lights up everything except itself.
        let is_target = match &self.drag {
            Some(DragSubject::Panel(d)) => *d != kind,
            Some(tab @ DragSubject::Tab { .. }) => tab.is_active() && kind.is_editor(),
            None => false,
        };
        if is_target {
            let hovered = self
                .drop_target
                .filter(|(t, _, _)| *t == kind)
                .map(|(_, s, _)| s);
            let overlay = responsive(move |size| {
                let (w, h) = (size.width, size.height);
                let (tw, th) = (tree.width, tree.height);
                let sensor =
                    iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
                        .on_move(move |p| Message::PanelHover(kind, p.x, p.y, w, h, tw, th))
                        .on_release(Message::PanelDrop);

                // Each zone is drawn at its true size — the same EDGE_BAND fraction the hit test
                // uses — so what you see is exactly what you can hit. A zone that spans the
                // layout (docking a full column/row) reads brighter than one that merely splits
                // this panel, because the outcomes differ a lot.
                let band = sc_win::layout::Side::EDGE_BAND;
                let zone = |side: Side| {
                    let outer = side.is_outer(
                        // A point squarely inside this side's band, so `is_outer` answers for
                        // the zone rather than for the cursor.
                        match side {
                            Side::Left => 0.0,
                            Side::Right => w,
                            _ => w * 0.5,
                        },
                        match side {
                            Side::Top => 0.0,
                            Side::Bottom => h,
                            _ => h * 0.5,
                        },
                        w,
                        h,
                        tw,
                        th,
                    );
                    container(Space::new()).width(Fill).height(Fill).style(
                        match (hovered == Some(side), outer) {
                            (true, true) => drop_zone_active_span_style,
                            (true, false) => drop_zone_active_style,
                            (false, true) => drop_zone_span_style,
                            (false, false) => drop_zone_idle_style,
                        },
                    )
                };

                // Left | (top / centre / bottom) | right — the four bands laid out at their real
                // proportions, with the untargetable middle left clear.
                let middle = column![
                    zone(Side::Top).height(Length::FillPortion(pct(band))),
                    Space::new().height(Length::FillPortion(pct(1.0 - 2.0 * band))),
                    zone(Side::Bottom).height(Length::FillPortion(pct(band))),
                ]
                .width(Fill);
                let zones = row![
                    zone(Side::Left).width(Length::FillPortion(pct(band))),
                    container(middle)
                        .width(Length::FillPortion(pct(1.0 - 2.0 * band)))
                        .height(Fill),
                    zone(Side::Right).width(Length::FillPortion(pct(band))),
                ]
                .height(Fill);

                // Zones UNDER the sensor, so the mouse_area still receives every move.
                iced::widget::stack![zones, sensor].into()
            });
            body = iced::widget::stack![body, overlay].into();
        }

        container(column![header, body].width(Fill).height(Fill))
            .width(Fill)
            .height(Fill)
            .style(card_style)
            .into()
    }

    /// One panel, sized by the caller.
    ///
    /// **Every arm returns `Fill × Fill` with no padding or card style of its own** — the walker
    /// applies those. That uniformity is what makes a panel placeable anywhere; the old
    /// signatures baked their own `FillPortion` in, which is why panel identity used to be
    /// positional.
    pub(crate) fn view_panel(&self, kind: PanelKind) -> Element<'_, Message> {
        match kind {
            PanelKind::Files => self.view_files_tab(),
            PanelKind::Git => self.view_git_panel(),
            PanelKind::Editor(id) => self.view_code_panel(id),
            PanelKind::Bottom => self.view_bottom_panel(),
            // A swarm build in flight: the live topology IS the story, so it takes the chat
            // panel's place for the duration — the same swap the fixed layout used to do.
            PanelKind::Chat => {
                if self.plan.started() && self.is_swarm() {
                    self.view_topology()
                } else {
                    self.view_center()
                }
            }
        }
    }
}

/// A panel's inner padding.
///
/// Deliberately tighter than the general [`PAD`]: panels butt directly against each other (the
/// gutter is zero), so this padding appears TWICE at every seam. At 8 that read as a 16px trough
/// between panels; at 3 the seam is a line rather than a gap, and content still doesn't touch
/// the edge.
pub(crate) const PANEL_PAD: u16 = 3;

/// The window-edge dock band's thickness.
///
/// One constant for two jobs: the bands' size, and how far the tree is inset while dragging. They
/// must match exactly — that's what makes the layout look like it's making room for the frame
/// rather than being covered by it.
pub(crate) const DOCK_BAND: f32 = 46.0;

/// A 0..1 fraction as `FillPortion` units (per mille), so the drop zones are laid out at exactly
/// the proportions the hit test uses — what you see is what you can hit.
fn pct(f: f32) -> u16 {
    (f * 1000.0).round().clamp(1.0, 1000.0) as u16
}

/// The default fraction for a split id, so a fresh install matches the old fixed layout exactly.
///
/// These are the proportions the hardcoded layout used; a user upgrading sees no visual change,
/// but every divider is now draggable.
fn default_frac(id: &str) -> f32 {
    match id {
        // The explorer was a fixed 200/1000 of the window.
        sc_win::splits::id::EXPLORER_BODY => 0.20,
        // The bottom strip was a fixed 180px; ~0.78 of a default-height window is the same look.
        sc_win::splits::id::BODY_BOTTOM => 0.78,
        sc_win::splits::id::EXPLORER_GIT_FILES => 0.25,
        _ => 0.5,
    }
}
