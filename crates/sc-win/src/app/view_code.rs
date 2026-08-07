//! App view: the code editor pane.

use super::*;
use iced::widget::{column, row};

impl App {
    /// The right CODE column: the selected/followed file with line numbers, read-only,
    /// rendered like a VS Code editor. The gutter (line numbers) and the code are TWO
    /// side-by-side columns; wrapping is disabled so a long line scrolls horizontally
    /// instead of wrapping into — and interrupting — the number gutter. One vertical
    /// scroll (whole editor) + one horizontal scroll (the code column) is what you get.
    pub(crate) fn view_code(&self) -> Element<'_, Message> {
        // Which surface the active tab is showing (spec 21). The REVIEW view is the original
        // read-only pane — diffs, line comments, gates — and is untouched by the editor work.
        // The EDIT view is a real buffer. A tab that can't be edited never reaches the second
        // branch: `Tab::open` forces it to Review.
        let editing = self
            .active_tab()
            .is_some_and(|t| t.view == TabView::Edit && t.editable());
        let inner: Element<'_, Message> = if editing {
            self.view_edit_body()
        } else {
            self.view_review_body()
        };
        self.view_code_frame(inner)
    }

    /// The EDIT view: the editor over the active tab's buffer, with a status line beneath.
    fn view_edit_body(&self) -> Element<'_, Message> {
        let Some(tab) = self.active_tab() else {
            return Space::new().into();
        };
        let Some(editor) = tab.editor() else {
            return Space::new().into();
        };

        // Status line: where the caret is, how big the file is, and what will be written back.
        // The line endings and encoding are shown because they are decisions the editor made on
        // the user's behalf — a silent CRLF→LF rewrite is a whole-file diff, so it is stated.
        let (line, col) = editor.cursor_position();
        let status = row![
            text(format!("Ln {}, Col {}", line + 1, col + 1))
                .size(11)
                .color(FG_MUTED),
            text("·").size(11).color(FG_MUTED),
            text(tab.ending.label()).size(11).color(FG_MUTED),
            text("·").size(11).color(FG_MUTED),
            text(if tab.bom { "UTF-8 BOM" } else { "UTF-8" })
                .size(11)
                .color(FG_MUTED),
            Space::new().width(Fill),
            if tab.dirty {
                text("● unsaved — Ctrl+S").size(11).color(AMBER)
            } else {
                text("saved").size(11).color(FG_MUTED)
            },
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);

        let mut col_ = column![
            container({
                // Tag events with the pane that produced them, so keystrokes reach THAT
                // buffer rather than whichever pane happens to hold focus.
                let pane = self.panes.focused_id();
                editor.view().map(move |ev| Message::EditorEvent(pane, ev))
            })
            .height(Fill),
            status,
        ]
        .spacing(4)
        .height(Fill);

        // A refused save is surfaced here, right under the buffer it refers to — and it offers
        // the only two honest answers. It is never resolved automatically: both versions hold
        // real work, so the user picks.
        if self.save_conflict.as_deref() == Some(tab.path.as_str()) {
            col_ = col_.push(self.view_save_conflict());
        }
        col_.into()
    }

    /// The save-refused notice: what happened, and the two ways out.
    fn view_save_conflict(&self) -> Element<'_, Message> {
        container(
            column![
                text("Not saved — this file changed on disk since you opened it.")
                    .size(12)
                    .color(AMBER),
                text(
                    "Something else (probably the agent) wrote to it while you had unsaved \
                     edits. Overwriting discards their version; dismissing keeps yours in the \
                     buffer, unsaved.",
                )
                .size(11)
                .color(FG_MUTED),
                row![
                    button(text("Overwrite the file").size(12))
                        .on_press(Message::OverwriteOnConflict)
                        .padding([3, 10])
                        .style(menu_item_style),
                    button(text("Keep editing").size(12))
                        .on_press(Message::DismissSaveConflict)
                        .padding([3, 10])
                        .style(primary_button),
                ]
                .spacing(8),
            ]
            .spacing(6),
        )
        .padding(8)
        .width(Fill)
        .style(card_style)
        .into()
    }

    /// The REVIEW view: the original read-only pane. Unchanged by the editor work — it carries
    /// the diff washes, inline comments and gate affordances that no editor widget can host.
    fn view_review_body(&self) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match &self.panes.focused().code {
            Some(cv) if cv.note.is_some() => text(cv.note.clone().unwrap_or_default())
                .size(12)
                .color(FG_MUTED)
                .into(),
            Some(cv) => {
                // Per-line rows you can DRAG across to select a range, then comment (PR-style).
                // Each line is a mouse_area (press=start drag, enter=extend, release=commit)
                // wrapping ONE no-wrap monospace string; selected lines get an accent wash.
                // The comment box renders after the last line of the committed range.
                let sel = self.selected_line_range(); // active drag OR committed range
                                                      // The amber "working" range applies only when the shown file is the one being
                                                      // worked on. A pulsing alpha (sine on the animation clock) reads as "in progress".
                let working_here = self
                    .working
                    .as_ref()
                    .filter(|(f, _, _)| {
                        Some(f.as_str()) == self.panes.focused().selected_file.as_deref()
                    })
                    .map(|(_, lo, hi)| (*lo, *hi));
                let pulse = 0.10 + 0.10 * (0.5 + 0.5 * (self.now() * 3.0).sin());

                // Always window to the visible lines (fast on big files). Inline comments and the
                // open comment box have variable height, but they only ever appear INSIDE the
                // rendered window (anchored to a visible line), between the two spacers — so they
                // never corrupt the spacer counts, which only stand in for plain CODE_LINE_PX
                // rows above/below. A box below shifts later lines slightly, exactly as before.
                // When the minimap is floating (file overflows the viewport), inline-comment rows
                // need right padding so their ✕/revert buttons don't hide behind it. 72px minimap
                // + a small gap. Computed here since the comment rows are built in the loop below.
                let minimap_overflows = self
                    .panes
                    .focused()
                    .code_viewport
                    .is_some_and(|(_, height)| height < 0.999);
                // Width for the comment / revert bars: the VIEWPORT width (not the horizontally-
                // scrollable content width), minus the minimap gutter when it's floating, so a bar
                // spans the visible area and ends just before the minimap — with round edges. It
                // scales with the window and expands when the minimap is hidden. `None` (→ Fill)
                // before the first scroll gives us a real viewport width.
                let bar_width: Option<f32> = (self.panes.focused().code_view_w > 1.0).then(|| {
                    let gutter = if minimap_overflows { 76.0 } else { 8.0 };
                    (self.panes.focused().code_view_w - gutter).max(120.0)
                });

                // Diff blocks (VS-Code-style). Two derived maps:
                //  • `block_bar_after`: last green line of a block → its cur_start (where to render
                //    the standalone "↩ revert block" bar).
                //  • `line_to_block`: every green line → its block's cur_start (so a comment ON a
                //    changed block can offer the same revert inline).
                let hunks = self.panes.focused().file_diff.hunks();
                let block_bar_after: std::collections::BTreeMap<usize, usize> = hunks
                    .iter()
                    .filter(|h| h.cur_start <= h.cur_end) // has a current (green) line
                    .map(|h| (h.cur_end, h.cur_start))
                    .collect();
                let line_to_block: std::collections::BTreeMap<usize, usize> = hunks
                    .iter()
                    .filter(|h| h.cur_start <= h.cur_end)
                    .flat_map(|h| (h.cur_start..=h.cur_end).map(move |l| (l, h.cur_start)))
                    .collect();
                // Blocks that already have a comment on them — those revert from the comment row,
                // so we skip the standalone bar to avoid a duplicate button.
                let blocks_with_comment: std::collections::BTreeSet<usize> = self
                    .panes
                    .focused()
                    .selected_file
                    .as_deref()
                    .map(|f| {
                        self.comments
                            .on_file(f)
                            .filter_map(|(_, c)| {
                                (c.start..=c.end).find_map(|l| line_to_block.get(&l).copied())
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let total = cv.lines.len();
                let (first_idx, last_idx) = {
                    const OVERSCAN: usize = 12;
                    let per = CODE_LINE_PX.max(1.0);
                    let view_h = if self.panes.focused().code_view_h > 1.0 {
                        self.panes.focused().code_view_h
                    } else {
                        800.0 // generous first-frame guess before we know the real height
                    };
                    let first = ((self.panes.focused().code_scroll_y / per) as usize)
                        .saturating_sub(OVERSCAN);
                    let visible = (view_h / per).ceil() as usize + 2 * OVERSCAN;
                    (first.min(total), (first + visible).min(total))
                };

                let mut col = column![].spacing(0);
                // Top spacer standing in for the hidden lines above the window (keeps the
                // scrollbar geometry, minimap box, and scroll_to offsets pixel-accurate). It must
                // also cover any RED removed-lines that anchor above the window (they render as
                // rows, so they add height) — count removals before the first visible line.
                if first_idx > 0 {
                    let first_visible_line = first_idx + 1; // 1-based line number of cv.lines[first_idx]
                    let hidden_removed: usize = self
                        .panes
                        .focused()
                        .file_diff
                        .removed_before
                        .range(..first_visible_line)
                        .map(|(_, v)| v.len())
                        .sum();
                    let hidden_rows = first_idx + hidden_removed;
                    col = col.push(
                        Space::new().height(Length::Fixed(hidden_rows as f32 * CODE_LINE_PX)),
                    );
                }
                for (n, line) in &cv.lines[first_idx..last_idx] {
                    // GitHub-PR style: render any lines DELETED just before this one as red rows
                    // (a `-` gutter, red wash). They exist only in HEAD, so they're not selectable.
                    if let Some(removed) = self.panes.focused().file_diff.removed_before.get(n) {
                        for gone in removed {
                            col = col.push(
                                container(
                                    text(format!("-     {gone}"))
                                        .size(13)
                                        .line_height(iced::widget::text::LineHeight::Absolute(
                                            CODE_LINE_PX.into(),
                                        ))
                                        .font(iced::Font::MONOSPACE)
                                        .color(BAD)
                                        .wrapping(iced::widget::text::Wrapping::None),
                                )
                                .height(Length::Fixed(CODE_LINE_PX))
                                .padding([0, 4])
                                .style(|_t: &Theme| code_removed_line_container()),
                            );
                        }
                    }
                    let in_sel = sel.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    let working = working_here.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    // A line that differs from HEAD (git) → GitHub-PR-style green highlight
                    // with a `+` gutter marker, so you SEE what the agent changed, live.
                    let changed = self.panes.focused().changed_lines.contains(n);
                    let mark = if changed { "+" } else { " " };
                    let row_text = format!("{mark}{n:>4}  {line}");
                    let color = if in_sel {
                        ACCENT
                    } else if changed {
                        GOOD
                    } else if working {
                        AMBER
                    } else {
                        FG
                    };
                    let line_el = container(
                        text(row_text)
                            .size(13)
                            // Pin the text's line height so every row is EXACTLY CODE_LINE_PX
                            // tall — the fixed height the minimap, scroll-jump, and the
                            // virtualization spacers all assume. Without this, natural line
                            // height drifts from the estimate and windowed scrolling desyncs.
                            .line_height(iced::widget::text::LineHeight::Absolute(
                                CODE_LINE_PX.into(),
                            ))
                            .font(iced::Font::MONOSPACE)
                            .color(color)
                            .wrapping(iced::widget::text::Wrapping::None),
                    )
                    .height(Length::Fixed(CODE_LINE_PX))
                    .padding([0, 4])
                    .style(move |_t: &Theme| {
                        code_line_container(in_sel, changed, working.then_some(pulse))
                    });
                    let row_ma = iced::widget::mouse_area(line_el)
                        .on_press(Message::LineDragStart(*n))
                        .on_enter(Message::LineDragTo(*n))
                        .on_release(Message::LineDragEnd);
                    col = col.push(row_ma);
                    // After the LAST green line of a changed block, render a "↩ revert block" bar —
                    // its own comment-shaped row (no comment text) carrying only the revert button,
                    // so the control lives on a dedicated line instead of floating over code. Skip
                    // it when a comment already sits on this block (it offers revert inline).
                    if let Some(&cur_start) = block_bar_after.get(n) {
                        if !blocks_with_comment.contains(&cur_start) {
                            col = col.push(view_revert_block_bar(cur_start, bar_width));
                        }
                    }
                    // Stored inline comments whose range ENDS on this line — render them (PR
                    // style), struck-through + ✓ once resolved. Only the in-window lines are
                    // iterated, so a comment scrolled off-screen simply isn't drawn (its state
                    // persists); the box/comment adds height inside the window, not the spacers.
                    if let Some(file) = self.panes.focused().selected_file.clone() {
                        let here: Vec<(usize, sc_win::comments::Comment)> = self
                            .comments
                            .on_file(&file)
                            .filter(|(_, c)| c.end == *n)
                            .map(|(i, c)| (i, c.clone()))
                            .collect();
                        for (i, c) in here {
                            // If the comment sits on a changed block, offer to revert that block
                            // from the comment row (look up by any line the comment covers).
                            let block =
                                (c.start..=c.end).find_map(|l| line_to_block.get(&l).copied());
                            col = col.push(view_inline_comment(i, c, bar_width, block));
                        }
                    }
                    // The (new) comment box after the last line of the committed range.
                    if self
                        .panes
                        .focused()
                        .comment_range
                        .is_some_and(|(_, hi)| hi == *n)
                    {
                        col = col.push(self.view_comment_box());
                    }
                }
                // Bottom spacer for the hidden lines below the window — plus any removed-lines that
                // anchor below the last visible line (they'd add height when scrolled into view).
                if last_idx < total {
                    let first_hidden_line = last_idx + 1; // 1-based line after the window
                    let below_removed: usize = self
                        .panes
                        .focused()
                        .file_diff
                        .removed_before
                        .range(first_hidden_line..)
                        .map(|(_, v)| v.len())
                        .sum();
                    let hidden_rows = (total - last_idx) + below_removed;
                    col = col.push(
                        Space::new().height(Length::Fixed(hidden_rows as f32 * CODE_LINE_PX)),
                    );
                } else {
                    // Window reaches EOF: render removals anchored past the last line (deletions at
                    // end-of-file) as trailing red rows.
                    for (anchor, removed) in self
                        .panes
                        .focused()
                        .file_diff
                        .removed_before
                        .range(total + 1..)
                    {
                        let _ = anchor;
                        for gone in removed {
                            col = col.push(
                                container(
                                    text(format!("-     {gone}"))
                                        .size(13)
                                        .line_height(iced::widget::text::LineHeight::Absolute(
                                            CODE_LINE_PX.into(),
                                        ))
                                        .font(iced::Font::MONOSPACE)
                                        .color(BAD)
                                        .wrapping(iced::widget::text::Wrapping::None),
                                )
                                .height(Length::Fixed(CODE_LINE_PX))
                                .padding([0, 4])
                                .style(|_t: &Theme| code_removed_line_container()),
                            );
                        }
                    }
                }
                if cv.truncated {
                    col = col.push(
                        text(format!(
                            "… truncated at {} lines",
                            sc_win::codeview::MAX_LINES
                        ))
                        .size(11)
                        .color(FG_MUTED),
                    );
                }
                // Only show the minimap when the file actually overflows the viewport — if it all
                // fits on screen there's nothing to navigate, so the map is just noise. Computed
                // above as `minimap_overflows` (drives both the comment inset and the minimap).
                let overflows = minimap_overflows;
                // With the minimap up, its viewport box IS the vertical scrollbar — hide the real
                // one (width 0) so we don't show both. Keep the horizontal bar for long lines.
                let vbar = if overflows {
                    scrollable::Scrollbar::new().width(0).scroller_width(0)
                } else {
                    scrollable::Scrollbar::new()
                };
                // One scrollable, BOTH axes: vertical for the file, horizontal for long lines.
                let code_scroll = scrollable(col)
                    .id(code_scroll_id(self.panes.focused_id()))
                    .on_scroll(Message::CodeScrolled)
                    .direction(scrollable::Direction::Both {
                        vertical: vbar,
                        horizontal: scrollable::Scrollbar::new().width(6).scroller_width(6),
                    })
                    .height(Fill)
                    .width(Fill);
                // VS-Code-style minimap on the right: file shape + green changes + comment
                // ticks + viewport box; click a line in it to select there.
                let line_lens: Vec<usize> =
                    cv.lines.iter().map(|(_, t)| t.chars().count()).collect();
                let commented: std::collections::BTreeSet<usize> = self
                    .panes
                    .focused()
                    .selected_file
                    .as_deref()
                    .map(|f| {
                        self.comments
                            .on_file(f)
                            .flat_map(|(_, c)| c.start..=c.end)
                            .collect()
                    })
                    .unwrap_or_default();
                if overflows {
                    let minimap = crate::minimap::Minimap::new(
                        line_lens,
                        self.panes.focused().changed_lines.clone(),
                        commented,
                        self.panes.focused().code_viewport,
                    )
                    .view();
                    // VS-Code style: the code fills the whole width and the minimap floats
                    // semi-transparently on top of the right edge, so text runs behind it.
                    let floating = container(minimap)
                        .width(Fill)
                        .height(Fill)
                        .align_x(iced::alignment::Horizontal::Right);
                    iced::widget::stack![code_scroll, floating].into()
                } else {
                    code_scroll.into()
                }
            }
            None => text("the file the agent edits appears here — or click one in the tree")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };
        inner
    }

    /// The CODE panel's chrome: the tab strip, the Review|Edit toggle, and the pinned agent
    /// actions. Shared by both views so the frame can't drift between them.
    ///
    /// Returns `Fill × Fill` with no sizing of its own — the panel-tree walker positions it
    /// (spec 21). Taking a `portion` was what made panel identity positional.
    fn view_code_frame<'a>(&'a self, inner: Element<'a, Message>) -> Element<'a, Message> {
        // Header stays fixed; the code area is the single scrollable (no outer scroll wrap,
        // which is what previously collapsed the inner one). The header is now a TAB STRIP — one
        // tab per open file, the ACTIVE tab (=== `selected_file`) highlighted, each closeable.
        // When the active file is a feature plan (PLAN-<slug>.md) and no session is running, the
        // strip's right end carries an "⚒ Execute plan" button — the same one-click build the
        // proposal card offers, acting on the active file.
        // "This is a feature plan, offer Breakdown/Build" — an agent action, so never in Craft
        // mode. A spec markdown file is just a file to read and edit there.
        let is_open_plan = !self.cfg.craft()
            && self
                .panes
                .focused()
                .selected_file
                .as_deref()
                .is_some_and(is_feature_plan);
        let header_bar: Element<'_, Message> = if self.panes.focused().tabs.is_empty() {
            // No files open → the old "CODE" placeholder (matches the former (None, _) header).
            text("CODE").size(12).color(FG_MUTED).into()
        } else {
            // One tab per open file, in open order. Each tab is a label button (switches to it)
            // plus a SIBLING ✕ button (buttons can't nest) that closes it. The active tab reads
            // in ACCENT on a card-ish wash; inactive tabs are FG_MUTED and transparent.
            let mut strip = row![].spacing(4).align_y(iced::Alignment::Center);
            for tab in &self.panes.focused().tabs {
                let path = &tab.path;
                let active = self.panes.focused().selected_file.as_deref() == Some(path.as_str());
                // Show the basename (full path is too long for a tab); duplicate basenames across
                // open files are acceptable for v1. A leading ● marks unsaved edits — the one
                // signal that says "closing this loses something".
                let base = path.rsplit(['/', '\\']).next().unwrap_or(path.as_str());
                let shown = if tab.dirty {
                    format!("● {base}")
                } else {
                    base.to_string()
                };
                // The label and its ✕ are separate buttons (buttons can't nest), so the FILL goes
                // on a container wrapping both — styling only the label left the ✕ sitting on a
                // different colour, which showed as a notch out of the active tab.
                let label =
                    button(
                        text(shown)
                            .size(12)
                            .color(if active { ACCENT } else { FG_MUTED }),
                    )
                    .on_press(Message::SelectTab(path.clone()))
                    .padding([6, 8])
                    .style(tab_label_button);
                let close =
                    button(
                        text("✕")
                            .size(11)
                            .color(if active { ACCENT } else { FG_MUTED }),
                    )
                    .on_press(Message::CloseTab(path.clone()))
                    .padding([6, 6])
                    .style(tab_label_button);
                let mut tab = container(
                    row![label, close]
                        .spacing(0)
                        .align_y(iced::Alignment::Center),
                );
                // The active tab carries the editor's own background so it reads as continuous
                // with the document beneath, rather than as one more button on the strip.
                if active {
                    tab = tab.style(|_t: &Theme| container::Style {
                        background: Some(Background::Color(EDITOR_BG)),
                        ..container::Style::default()
                    });
                }
                strip = strip.push(tab);
            }
            // Build the header ACTION buttons first (their own fixed-width row), so they can be
            // PINNED to the right while the tab strip scrolls in the remaining space — VS Code
            // style. Without this, the scroller expanded to fit every tab and pushed the buttons
            // off the panel's right edge (the bug: Build/Breakdown vanished with many tabs open).
            // Every action in this row drives the agent — approving a phase, sending it back,
            // or starting a build. None of them exist in Craft mode (spec 21), so the row stays
            // empty and the tab strip gets the full width.
            let viewing_gated_phase = !self.cfg.craft()
                && self.gating_phase().is_some_and(|p| {
                    self.plan.path_for(p).as_deref()
                        == self.panes.focused().selected_file.as_deref()
                });
            let mut actions = row![].spacing(8).align_y(iced::Alignment::Center);
            // Review|Edit, on every editable tab. One control, both directions — reading a diff
            // and typing are separate surfaces (see `view_code`), and this is how you move
            // between them. A tab that cannot be edited shows nothing rather than a dead toggle.
            if let Some(tab) = self.active_tab().filter(|t| t.editable()) {
                let editing = tab.view == TabView::Edit;
                actions = actions.push(
                    button(
                        text(if editing { "◧ Review" } else { "✎ Edit" })
                            .size(12)
                            .color(FG_MUTED),
                    )
                    .on_press(Message::ToggleTabView)
                    .padding([3, 10])
                    .style(menu_item_style),
                );
                if editing && tab.dirty {
                    actions = actions.push(
                        button(text("Save").size(12))
                            .on_press(Message::SaveFile)
                            .padding([3, 10])
                            .style(primary_button),
                    );
                }
            }
            if viewing_gated_phase {
                // The file being viewed IS the phase at a gate: its Approve / Send back / Abort
                // controls sit here so you review the artifact and act in the same place (Send back
                // harvests this file's line-comments as the revision notes).
                actions = actions.push(
                    button(text("✓ Approve").size(12))
                        .on_press(Message::GateApprove)
                        .padding([3, 10])
                        .style(primary_button),
                );
                actions = actions.push(
                    button(text("↩ Send back").size(12))
                        .on_press(Message::GateSendBack)
                        .padding([3, 10])
                        .style(menu_item_style),
                );
                actions = actions.push(
                    button(text("■ Abort").size(12))
                        .on_press(Message::GateAbort)
                        .padding([3, 10])
                        .style(menu_item_style),
                );
            } else if is_open_plan && self.session.is_none() {
                // Two actions on an open plan: Breakdown runs the staged DESIGN pipeline and stops
                // for review (no code written); Build runs the whole thing through to a green
                // compile. Breakdown first — it's the review-then-build path.
                actions = actions.push(
                    button(text("☷ Breakdown").size(12))
                        .on_press(Message::ExecuteOpenPlan)
                        .padding([3, 10])
                        .style(menu_item_style),
                );
                actions = actions.push(
                    button(text("⚒ Build").size(12))
                        .on_press(Message::BuildOpenPlan)
                        .padding([3, 10])
                        .style(primary_button),
                );
            }
            // The strip scrolls horizontally in the space LEFT OF the pinned actions: `width(Fill)`
            // makes the scroller take the remaining width (not grow to fit every tab), so overflow
            // scrolls inside it while the action buttons keep their natural width on the right.
            // A visible horizontal scrollbar as the affordance that the strip scrolls when tabs
            // overflow (the old 2px bar was invisible — it looked like the extra tabs were just
            // gone). The bar is drawn along the BOTTOM edge of the scrollable's viewport, so it
            // would overlap the tab text; reserve a lane for it with bottom padding on the strip
            // content, and keep tabs top-aligned so they sit ABOVE the bar, not centered onto it.
            // Mouse-wheel over the strip scrolls it horizontally too.
            // Tabs stretch to the strip's full height and reach its BOTTOM edge, so the active
            // tab meets the document with no seam. The scrollbar lane that used to be reserved
            // here (9px of bottom padding) showed as a gap under every tab — the bar now
            // overlays the strip instead, which costs nothing since it only appears on overflow.
            let strip = strip.align_y(iced::Alignment::Center);
            let scroller = scrollable(strip)
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new()
                        .width(3)
                        .scroller_width(3)
                        .margin(0),
                ))
                .width(Fill);
            // The row MUST be `width(Fill)`: in a Shrink row, a `Fill` child resolves against the
            // row's content width (unbounded), so the scroller grows to fit every tab and shoves
            // the actions off-screen. A Fill row bounds the space, the Shrink `actions` take their
            // natural width, and the Fill scroller gets what's left → tabs scroll, buttons pinned.
            row![scroller, actions]
                .spacing(8)
                .width(Fill)
                .align_y(iced::Alignment::Center)
                .into()
        };
        // The body column must be `Fill`-width so `header_bar`'s Fill row has a bounded width to
        // fill — a Shrink column would collapse it to content width and the tab-scroll/pinned-
        // buttons layout would break (the buttons get pushed off again).
        // No padding or card style here — the tree walker applies both, so every panel is
        // framed identically wherever it sits.
        // The document area sits on the EDITOR canvas in BOTH views — a shade darker than the
        // panel, so the code reads as recessed. Applied here rather than per-view so toggling
        // Review/Edit doesn't change the background under you.
        //
        // No spacing between the strip and the body: the active tab is filled with this same
        // colour, so they meet as one surface. A gap would break that join.
        column![
            header_bar,
            container(inner)
                .width(Fill)
                .height(Fill)
                .style(|_t: &Theme| container::Style {
                    background: Some(Background::Color(EDITOR_BG)),
                    ..container::Style::default()
                }),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }
}
