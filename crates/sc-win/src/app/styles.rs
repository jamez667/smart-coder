//! Visual design tokens + reusable widget/style builders.

use super::*;

// --- Visual design tokens (Tokyo Night-aligned) -----------------------------------
// A small, consistent palette + spacing so panels read as cards on a dark canvas,
// not bare text floating on the background.

/// Panel/card surface — a hair lighter than the window background.
pub(crate) const SURFACE: Color = Color::from_rgb(0.106, 0.118, 0.18);
/// Input field fill — a touch lighter than [`SURFACE`], so the composer input stands out
/// gently from the panel behind it.
pub(crate) const INPUT_BG: Color = Color::from_rgb(0.15, 0.165, 0.24);
/// The editor's own canvas — a shade DARKER than [`SURFACE`].
///
/// The document you're editing should read as recessed into the panel rather than flush with the
/// chrome around it: the tab strip and status line sit on [`SURFACE`], the text sits in here.
/// Darker rather than lighter follows every editor's convention, and keeps syntax colours
/// carrying more contrast than the UI around them.
pub(crate) const EDITOR_BG: Color = Color::from_rgb(0.075, 0.085, 0.135);
/// A subtle border around cards.
pub(crate) const CARD_BORDER: Color = Color::from_rgb(0.20, 0.22, 0.32);
/// Primary text.
pub(crate) const FG: Color = Color::from_rgb(0.84, 0.86, 0.93);
/// Muted / secondary text (section labels, hints).
pub(crate) const FG_MUTED: Color = Color::from_rgb(0.52, 0.55, 0.66);
/// Accent (the build action, current step, active flows).
pub(crate) const ACCENT: Color = Color::from_rgb(0.48, 0.65, 0.98);
pub(crate) const GOOD: Color = Color::from_rgb(0.45, 0.78, 0.55);
pub(crate) const BAD: Color = Color::from_rgb(0.93, 0.45, 0.50);
/// Amber — the "agent is working on these lines" highlight (pulses while a fix is in flight).
pub(crate) const AMBER: Color = Color::from_rgb(0.95, 0.72, 0.35);
/// Orange — the primary action buttons (Send / build / iterate / Execute). A warm, confident
/// call-to-action against the cool dark canvas.
pub(crate) const ORANGE: Color = Color::from_rgb(0.96, 0.55, 0.24);

// --- Spacing / shape tokens (the modern flat look) --------------------------------
// One radius and one panel padding, so the whole UI reads as a single system and the
// look is dialed from here — not scattered magic numbers. Panels butt together with a
// hairline gutter instead of floating as rounded cards on a wide gap.

/// The single corner radius for every widget — fully square for a flat, modern look.
pub(crate) const RADIUS: f32 = 0.0;
/// The gutter between panels: none — panels butt seamlessly against each other. The
/// card border alone divides them.
pub(crate) const GAP: f32 = 0.0;
/// Shared inner padding for the main panels — tighter than the old 10/12 so the cramped
/// left tree reclaims width.
pub(crate) const PAD: u16 = 8;

/// A stable id for the code-view scrollable, so the minimap can scroll it to a clicked line
/// via `iced::widget::operation::scroll_to`.
pub(crate) fn code_scroll_id(pane: sc_win::layout::EditorId) -> iced::advanced::widget::Id {
    // PER PANE, not a singleton. `scroll_to` addresses a widget by id, so two panes sharing one
    // id means jumping to a line in either scrolls BOTH — a silent bug that only appears once a
    // second pane exists, which is why it survived until now.
    // `Id::new` takes `&'static str`; a per-pane id is built at runtime, so go through
    // `From<String>` (which stores an owned `Cow`) instead.
    iced::advanced::widget::Id::from(format!("code-view:{pane}"))
}

/// A stable id for the chat thread scrollable, so we can keep it pinned to the bottom as new
/// messages stream in (unless the user has scrolled up to read).
pub(crate) fn chat_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("chat-thread")
}

/// Approx. pixel height of one rendered code line (size-13 monospace) — used to convert a
/// clicked minimap line into a scroll offset.
pub(crate) const CODE_LINE_PX: f32 = 17.0;

/// Card surface style: a flat filled panel. Borderless — panels are separated by explicit
/// 1px [`v_divider`]/[`h_divider`] lines between them, so an interior seam is exactly one
/// pixel (two touching card borders would have doubled it to 2px).
pub(crate) fn card_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        border: Border {
            radius: RADIUS.into(),
            // NO outline. The draggable divider between panels already draws the seam, so a
            // per-panel border stacked two lines at every join — visible as a double edge with a
            // trough between them. One line per seam, owned by the divider.
            ..Default::default()
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A panel's drag header — a shade lighter than the panel body so the grab strip reads as
/// distinct from the content without becoming a stripe of chrome.
pub(crate) fn panel_header_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.145, 0.161, 0.235))),
        border: Border {
            radius: iced::border::radius(RADIUS).bottom(0.0),
            ..Default::default()
        },
        text_color: Some(FG_MUTED),
        ..container::Style::default()
    }
}

/// The header of the panel currently being dragged — accent-tinted, so it's obvious which one is
/// in flight while the cursor is somewhere else entirely.
pub(crate) fn panel_header_dragging_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.35, ..ACCENT })),
        border: Border {
            radius: iced::border::radius(RADIUS).bottom(0.0),
            ..Default::default()
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// The drag ghost: a small floating copy of the panel being moved, following the cursor.
///
/// Opaque rather than translucent, with an accent border — it has to read as a solid thing you
/// are carrying, over arbitrary panel content underneath. A see-through ghost over a busy file
/// tree is illegible.
pub(crate) fn drag_ghost_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        border: Border {
            radius: RADIUS.into(),
            width: 2.0,
            color: Color { a: 0.95, ..ACCENT },
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A window-edge dock band at rest.
///
/// These form a frame around the WHOLE layout and dock a full-width row or full-height column,
/// unlike the per-panel zones below which only split one panel. Distinctly stronger than an idle
/// per-panel zone so the frame reads as its own surface rather than more of the same.
pub(crate) fn dock_band_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.22, ..ACCENT })),
        border: Border {
            radius: 4.0.into(),
            width: 1.0,
            color: Color { a: 0.45, ..ACCENT },
        },
        text_color: Some(FG_MUTED),
        ..container::Style::default()
    }
}

/// The dock band under the cursor — the strongest state in the drag UI, because it does the most.
pub(crate) fn dock_band_active_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.72, ..ACCENT })),
        border: Border {
            radius: 4.0.into(),
            width: 2.0,
            color: Color { a: 1.0, ..ACCENT },
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// The four drop-zone styles, shown on every panel while a drag is in flight.
///
/// All four zones of every target are drawn at once, at the same proportions the hit test uses —
/// revealing them only on hover meant you had to know where to aim before you could see where to
/// aim. Two axes of emphasis:
///
/// * **idle vs active** — active is the one under the cursor.
/// * **split vs span** — a *span* zone docks a full column/row across the whole layout; a *split*
///   zone only divides that one panel. Very different outcomes, so they never look alike.
///
/// Idle zones are deliberately faint: legible as "you may drop here", quiet enough that four of
/// them on every panel don't drown the UI.
pub(crate) fn drop_zone_idle_style(_t: &Theme) -> container::Style {
    drop_zone(0.10, false)
}

/// A span zone at rest — brighter than a split zone, because it does more.
pub(crate) fn drop_zone_span_style(_t: &Theme) -> container::Style {
    drop_zone(0.18, false)
}

/// The split zone under the cursor.
pub(crate) fn drop_zone_active_style(_t: &Theme) -> container::Style {
    drop_zone(0.45, true)
}

/// The span zone under the cursor — the strongest state there is.
pub(crate) fn drop_zone_active_span_style(_t: &Theme) -> container::Style {
    drop_zone(0.65, true)
}

/// Shared shape for the drop zones: an accent wash, outlined when active so the target reads as
/// a region rather than a tint.
fn drop_zone(alpha: f32, active: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: alpha, ..ACCENT })),
        border: Border {
            radius: 3.0.into(),
            width: if active { 1.5 } else { 0.0 },
            color: Color { a: 0.9, ..ACCENT },
        },
        ..container::Style::default()
    }
}

/// A 1px vertical hairline between side-by-side panels, in the card-border tone.
pub(crate) fn v_divider<'a>() -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fixed(1.0))
        .height(Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(CARD_BORDER)),
            ..container::Style::default()
        })
        .into()
}

/// A draggable version of [`v_divider`]: the same 1px hairline, but wrapped in a wider
/// invisible grab strip that shows the horizontal-resize cursor and starts a divider drag
/// on mouse-down. Used only between the chat and code panels.
///
/// Takes the grab message so every split in the panel tree carries its own node id and extent
/// (spec 21) — previously there was one global `SplitDragStart` because there was exactly one
/// draggable vertical divider.
pub(crate) fn v_divider_draggable<'a>(grab: Message) -> Element<'a, Message> {
    // A 1px visible line centred in a 3px hit strip. The strip used to be 7px, which read as a
    // trough between panels rather than a seam — at 3px the panels butt together with a single
    // hairline, and the target is still wide enough to grab. Filled with the panel SURFACE so
    // the pixels either side of the line match the panels they border, rather than showing the
    // darker window background as a faint band.
    let handle = container(v_divider())
        .width(Length::Fixed(3.0))
        .height(Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(SURFACE)),
            ..container::Style::default()
        });
    iced::widget::mouse_area(handle)
        .on_press(grab)
        .interaction(iced::mouse::Interaction::ResizingHorizontally)
        .into()
}

/// A 1px horizontal hairline between stacked panels, in the card-border tone.
pub(crate) fn h_divider<'a>() -> Element<'a, Message> {
    container(Space::new())
        .width(Fill)
        .height(Length::Fixed(1.0))
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(CARD_BORDER)),
            ..container::Style::default()
        })
        .into()
}

/// A draggable version of [`h_divider`]: the same 1px hairline, but wrapped in a taller
/// invisible grab strip that shows the vertical-resize cursor and starts a drag on mouse-down.
/// Used between the Git and Files sections of the explorer column to resize their split.
///
/// Takes the grab message, as [`v_divider_draggable`] does.
pub(crate) fn h_divider_draggable<'a>(grab: Message) -> Element<'a, Message> {
    // A 1px visible line centred in a 3px hit strip — see [`v_divider_draggable`]: 7px read as a
    // trough between stacked panels rather than a seam. Filled with the panel SURFACE so the
    // pixels either side match the sections it borders.
    let handle = container(h_divider())
        .width(Fill)
        .height(Length::Fixed(3.0))
        .align_y(iced::alignment::Vertical::Center)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(SURFACE)),
            ..container::Style::default()
        });
    iced::widget::mouse_area(handle)
        .on_press(grab)
        .interaction(iced::mouse::Interaction::ResizingVertically)
        .into()
}

/// Primary (accent-filled) button style for the build action.
pub(crate) fn primary_button(_t: &Theme, status: button::Status) -> button::Style {
    // A clean, crisp orange action button: solid fill that brightens on hover and dims on press.
    // No shadow or fake bevel — flat and modern, matching the rest of the UI. The label is
    // centered by the caller (a Fill-sized, centered text).
    let bg = match status {
        button::Status::Hovered => Color::from_rgb(1.0, 0.63, 0.33),
        button::Status::Pressed => Color::from_rgb(0.85, 0.47, 0.18),
        _ => ORANGE,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::from_rgb(0.12, 0.06, 0.02),
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A borderless, transparent button style for file-tree rows — so the explorer reads
/// as a clickable list, not a wall of buttons. A faint wash on hover gives feedback.
pub(crate) fn tree_button(_t: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Some(Background::Color(Color { a: 0.06, ..ACCENT })),
        _ => None,
    };
    button::Style {
        background: bg,
        text_color: FG,
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The top menu bar's background: a flat strip a touch darker than the cards.
pub(crate) fn menu_bar_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.08, 0.09, 0.14))),
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A menu-bar title button: transparent, faint wash when its dropdown is open or hovered.
pub(crate) fn menu_title_style(open: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    let bg = if open || hovered {
        Some(Background::Color(Color { a: 0.10, ..ACCENT }))
    } else {
        None
    };
    button::Style {
        background: bg,
        text_color: FG,
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A Windows-style menu item button: transparent, full accent-wash highlight on hover
/// (the classic "whole row highlights" behaviour), square corners for a native feel.
/// The little ＋ / − stage-toggle button on a git file row: a visible lighter surface with a
/// border so it reads as a clickable chip (the plain tree_button was nearly invisible), brightening
/// on hover.
pub(crate) fn stage_toggle_button(_t: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let a = if hovered { 0.28 } else { 0.16 };
    button::Style {
        background: Some(Background::Color(Color {
            a,
            ..Color::from_rgb(0.6, 0.64, 0.78)
        })),
        text_color: if hovered {
            FG
        } else {
            Color::from_rgb(0.85, 0.87, 0.94)
        },
        border: Border {
            color: Color {
                a: 0.35,
                ..CARD_BORDER
            },
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..Default::default()
    }
}

/// A file tab's label and its ✕ — transparent, so the tab's own container supplies the fill.
///
/// The two are separate buttons (buttons can't nest), so the active tab's background belongs on
/// a container wrapping both. Styling the buttons individually left the ✕ on a different colour,
/// which read as a notch bitten out of the tab.
pub(crate) fn tab_label_button(_t: &Theme, status: button::Status) -> button::Style {
    button::Style {
        // A faint wash on hover, so an inactive tab still responds to the pointer.
        background: matches!(status, button::Status::Hovered).then_some(Background::Color(Color {
            a: 0.35,
            ..INPUT_BG
        })),
        text_color: FG,
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub(crate) fn menu_item_style(_t: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then_some(Background::Color(ACCENT)),
        text_color: if hovered {
            Color::from_rgb(0.06, 0.07, 0.11)
        } else {
            FG
        },
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A code line container's wash, by state (precedence: selection → change → working):
///  • `selected` → faint accent (you're commenting on it),
///  • `changed` → faint GREEN (differs from HEAD — a git change, PR-style),
///  • `working = Some(alpha)` → pulsing AMBER (the agent is working these lines right now),
///  • else transparent.
pub(crate) fn code_line_container(
    selected: bool,
    changed: bool,
    working: Option<f32>,
) -> container::Style {
    let bg = if selected {
        Some(Background::Color(Color { a: 0.14, ..ACCENT }))
    } else if changed {
        Some(Background::Color(Color { a: 0.12, ..GOOD }))
    } else {
        working.map(|a| Background::Color(Color { a, ..AMBER }))
    };
    container::Style {
        background: bg,
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A GitHub-PR-style RED "removed line" row: a red wash behind the deleted (HEAD-only) text.
pub(crate) fn code_removed_line_container() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.12, ..BAD })),
        text_color: Some(BAD),
        ..container::Style::default()
    }
}

/// The floating dropdown card: an opaque surface with a border so it reads above the body.
pub(crate) fn dropdown_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.12, 0.13, 0.20))),
        border: Border {
            color: CARD_BORDER,
            width: 1.0,
            radius: RADIUS.into(),
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// Text-input style: the theme default, but with our single [`RADIUS`] so the boxes
/// match the flat panels instead of iced's rounder default corners.
pub(crate) fn input_style(t: &Theme, status: text_input::Status) -> text_input::Style {
    let mut s = text_input::default(t, status);
    s.border.radius = RADIUS.into();
    s
}

/// A borderless input style for the composer: the theme default with its border stripped and
/// its fill matched to the panel surface, so the field reads as part of the composer block
/// (filling the whole area) rather than a boxed widget floating inside it.
pub(crate) fn input_style_borderless(t: &Theme, status: text_input::Status) -> text_input::Style {
    let mut s = text_input::default(t, status);
    s.border.width = 0.0;
    s.border.radius = RADIUS.into();
    s.background = Background::Color(INPUT_BG);
    s
}

/// Checkbox style: the theme default squared to our [`RADIUS`].
pub(crate) fn checkbox_style(t: &Theme, status: checkbox::Status) -> checkbox::Style {
    let mut s = checkbox::primary(t, status);
    s.border.radius = RADIUS.into();
    s
}

/// A section header label (muted, uppercase-ish small caps feel via size).
pub(crate) fn section(label: &str) -> iced::widget::Text<'_> {
    text(label).size(12).color(FG_MUTED)
}

/// The shared "↩ revert" button used on both the standalone revert bar and inline-comment rows,
/// so they look and behave identically. Reverts the diff block starting at `cur_start`.
pub(crate) fn revert_button(cur_start: usize) -> Element<'static, Message> {
    button(text("↩ revert").size(11).color(GOOD))
        .on_press(Message::RevertBlock(cur_start))
        .padding([0, 8])
        .style(menu_item_style)
        .into()
}

/// The shared rounded, bordered bar chrome used by BOTH the standalone revert bar and inline
/// comments — one component so they're visually identical. Sized to the viewport (`bar_width`) so
/// the row ends before the minimap and its buttons stay visible without scrolling horizontally;
/// `tint` colours the faint background wash.
pub(crate) fn bar_container<'a>(
    content: impl Into<Element<'a, Message>>,
    bar_width: Option<f32>,
    tint: Color,
) -> Element<'a, Message> {
    let width = bar_width.map(Length::Fixed).unwrap_or(Fill);
    container(content)
        .width(width)
        .padding([4, 8])
        .style(move |_t: &Theme| container::Style {
            background: Some(Background::Color(Color { a: 0.07, ..tint })),
            border: Border {
                color: CARD_BORDER,
                width: 1.0,
                radius: RADIUS.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A standalone revert bar: the shared bar chrome carrying ONLY the "↩ revert" button (no comment
/// text). Rendered under a changed diff block that has no comment on it.
pub(crate) fn view_revert_block_bar(
    cur_start: usize,
    bar_width: Option<f32>,
) -> Element<'static, Message> {
    let head =
        row![Space::new().width(Fill), revert_button(cur_start)].align_y(iced::Alignment::Center);
    bar_container(head, bar_width, GOOD)
}

/// One stored inline comment rendered under its line (PR-style): a pending one shows the text + a
/// dismiss ✕; a resolved one shows a ✓ and dimmer text (the running "done" record). Uses the SAME
/// [`bar_container`] chrome + [`revert_button`] as the standalone bar. `revert_block_start`: if the
/// comment sits on a changed diff block, its `cur_start` so the row can offer a "↩ revert".
pub(crate) fn view_inline_comment(
    i: usize,
    c: sc_win::comments::Comment,
    bar_width: Option<f32>,
    revert_block_start: Option<usize>,
) -> Element<'static, Message> {
    let (mark, mark_color, txt_color) = if c.resolved {
        ("✓", GOOD, FG_MUTED)
    } else {
        ("💬", ACCENT, FG)
    };
    let tint = if c.resolved { GOOD } else { ACCENT };
    let can_revert = c.resolved && c.before.is_some();
    let mut head = row![
        text(mark).size(12).color(mark_color),
        text(c.text.clone()).size(12).color(txt_color),
        Space::new().width(Fill),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);
    // If the comment is on a changed git block, offer to revert that block right here — the same
    // shared button as the standalone bar.
    if let Some(cur_start) = revert_block_start {
        head = head.push(revert_button(cur_start));
    }
    // A resolved comment with stored before-text gets a per-comment Revert. While a revert is
    // available, that's the action to take — so we HIDE the ✕ (dismiss) until then, to steer you
    // to undo the change rather than silently drop the record. The ✕ shows on pending comments
    // (nothing applied yet) and on resolved ones with no revert available.
    if can_revert {
        head = head.push(
            button(text("↩ revert").size(11))
                .on_press(Message::RevertComment(i))
                .padding([0, 8])
                .style(menu_item_style),
        );
    } else {
        head = head.push(
            button(text("✕").size(11))
                .on_press(Message::DismissComment(i))
                .padding([0, 6])
                .style(menu_item_style),
        );
    }
    bar_container(head, bar_width, tint)
}
