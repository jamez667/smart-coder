//! The Profiler panel: record or open a profile, then read it as a flame graph.
//!
//! Its own file for the same reason [`super::view_comply`] is — `view_panels.rs` is already at
//! this crate's size ceiling.
//!
//! # The shape of the panel
//!
//! A toolbar, then the graph, then the answer. Reading top to bottom: *what am I looking at*,
//! *where did the time go* (the picture), *and which function was it* (the hot list). The hot
//! list is not decoration — a flame graph shows shape, but the question people actually arrive
//! with is answered by a sorted table, so both are on screen at once rather than behind a tab.
//!
//! # The empty state is the common one
//!
//! On a machine with no profiler installed — which is the default, and was true of the machine
//! this was built on — the panel still works: it opens a folded file from anywhere. So the
//! empty state leads with that, and treats the missing tool as a footnote with an install line,
//! not an error.

use super::*;
use iced::widget::{column, row};

use sc_win::flame::tool;

impl App {
    /// The zoomed-to subtree, falling back to the whole profile.
    ///
    /// A zoom path can go stale when a new profile is loaded under it; `at_path` returns `None`
    /// and the whole profile is shown, which is the harmless reading.
    fn flame_root<'a>(&self, p: &'a sc_win::flame::Profile) -> &'a sc_win::flame::Frame {
        if self.flame_zoom.is_empty() {
            return &p.root;
        }
        sc_win::flame::at_path(&p.root, &self.flame_zoom).unwrap_or(&p.root)
    }

    pub(crate) fn view_flame_panel(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match &self.flame_profile {
            Some(p) if !p.is_empty() => self.view_flame_graph(p),
            _ => self.view_flame_empty(),
        };

        column![
            self.view_flame_toolbar(),
            container(body).width(Fill).height(Fill),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }

    /// The toolbar: open, record, search, and the zoom breadcrumb.
    fn view_flame_toolbar(&self) -> Element<'_, Message> {
        let can_record =
            self.flame_tool.is_some() && self.project_kind == sc_win::project::ProjectKind::Cargo;

        // Record doubles as Stop while a run is in flight, so the button never lies about what
        // pressing it does.
        let record: Element<'_, Message> = if self.flame_running {
            button(text("■ Stop").size(11))
                .on_press(Message::CancelProfile)
                .padding([2, 8])
                .style(primary_button)
                .into()
        } else {
            let b = button(text("● Record").size(11))
                .padding([2, 8])
                .style(menu_item_style);
            // Disabled by withholding `on_press`, which iced renders as disabled — rather than
            // offering a button that reports a failure the panel already knows about.
            if can_record {
                b.on_press(Message::RecordProfile).into()
            } else {
                b.into()
            }
        };

        let mut bar = row![
            button(text("⏏ Open…").size(11))
                .on_press(Message::OpenProfile)
                .padding([2, 8])
                .style(menu_item_style),
            record,
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);

        if self.flame_profile.is_some() {
            bar = bar.push(
                text_input("search…", &self.flame_search)
                    .on_input(Message::FlameSearch)
                    .size(11)
                    .padding([2, 6])
                    .width(Length::Fixed(140.0)),
            );
            // Zoomed in: offer the way back out. Absent at full zoom, so the toolbar doesn't
            // carry a control that would do nothing.
            if !self.flame_zoom.is_empty() {
                bar = bar.push(
                    button(text("⤢ Reset zoom").size(11))
                        .on_press(Message::FlameZoom(Vec::new()))
                        .padding([2, 8])
                        .style(menu_item_style),
                );
            }
        }

        // The record controls: what to profile, and what to pass it. Shown only when recording
        // is actually possible — on a non-Cargo project or with no profiler they would be three
        // dead controls next to a disabled button, and the empty state already explains why.
        if can_record && !self.flame_running {
            bar = bar.push(
                button(text(self.flame_target.label()).size(11))
                    .on_press(Message::FlameTarget(next_target(&self.flame_target)))
                    .padding([2, 8])
                    .style(menu_item_style),
            );
            bar = bar.push(
                text_input("args after --", &self.flame_args)
                    .on_input(Message::FlameArgs)
                    .size(11)
                    .padding([2, 6])
                    .width(Length::Fixed(120.0)),
            );
        }

        bar = bar.push(Space::new().width(Fill));

        // The right-hand status: what is loaded, or what is happening.
        let status = if self.flame_running {
            text("recording…").size(11).color(AMBER)
        } else if let Some(p) = &self.flame_profile {
            text(format!("{} samples · {}", p.total(), self.flame_source))
                .size(11)
                .color(FG_MUTED)
        } else {
            text("no profile").size(11).color(FG_MUTED)
        };
        bar = bar.push(status);

        container(bar)
            .width(Fill)
            .padding([4, 8])
            .style(|_t: &Theme| container::Style {
                background: Some(Background::Color(SURFACE)),
                border: Border {
                    color: CARD_BORDER,
                    width: 1.0,
                    ..Border::default()
                },
                ..container::Style::default()
            })
            .into()
    }

    /// The graph, the detail line, and the hot-frames list.
    fn view_flame_graph<'a>(&'a self, p: &'a sc_win::flame::Profile) -> Element<'a, Message> {
        let root = self.flame_root(p);

        // The detail line: the hovered frame, or the search summary, or a hint. One line that
        // is always occupied, so the layout never jumps as the cursor moves.
        let detail: Element<'_, Message> = if let Some(h) = &self.flame_hover {
            let pct = h.percent_of(p.total());
            let own = if h.own > 0 {
                format!("  ·  {} self", h.own)
            } else {
                String::new()
            };
            row![
                text(h.name.clone()).size(11).color(FG),
                Space::new().width(Fill),
                text(format!("{:.2}%  ·  {} samples{}", pct, h.total, own))
                    .size(11)
                    .color(FG_MUTED),
            ]
            .into()
        } else if !self.flame_search.is_empty() {
            let pct = sc_win::flame::matched_percent(&p.root, &self.flame_search);
            text(format!(
                "“{}” matches {:.2}% of samples",
                self.flame_search, pct
            ))
            .size(11)
            .color(ACCENT)
            .into()
        } else {
            text("click a frame to zoom  ·  click the top row to zoom out")
                .size(11)
                .color(FG_MUTED)
                .into()
        };

        // The hot list: where the time actually went, worst first.
        let mut hot = column![text("Hottest frames (self time)").size(11).color(FG_MUTED)]
            .spacing(2)
            .padding(PAD)
            .width(Length::Fixed(260.0));
        let total = p.total().max(1);
        for (name, own) in sc_win::flame::hot_frames(&p.root, 12) {
            let pct = own as f32 / total as f32 * 100.0;
            hot = hot.push(
                row![
                    text(sc_win::flame::short_name(&name).to_string())
                        .size(11)
                        .color(FG)
                        .width(Fill),
                    text(format!("{pct:.1}%")).size(11).color(AMBER),
                ]
                .spacing(6),
            );
        }

        let graph = crate::flamecanvas::FlameCanvas::new(
            root,
            p.total(),
            &self.flame_search,
            self.flame_hover.as_ref(),
        )
        .view();

        let mut left = column![
            // The graph scrolls: a deep profile is taller than any panel.
            container(scrollable(graph)).width(Fill).height(Fill),
            container(detail)
                .width(Fill)
                .padding([3, 8])
                .style(|_t: &Theme| container::Style {
                    background: Some(Background::Color(SURFACE)),
                    ..container::Style::default()
                }),
        ]
        .width(Fill)
        .height(Fill);

        // Unreadable lines are worth a word — a file that parsed 3 stacks out of 4000 should
        // not look like a small profile.
        if p.skipped > 0 {
            left = left.push(
                container(
                    text(format!("{} unreadable lines were skipped", p.skipped))
                        .size(11)
                        .color(AMBER),
                )
                .padding([2, 8]),
            );
        }

        row![left, container(scrollable(hot)).height(Fill)]
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// Nothing loaded: say what this is and how to fill it.
    fn view_flame_empty(&self) -> Element<'_, Message> {
        let mut col = column![
            text("Profiler").size(14).color(FG),
            text("Open a folded-stack profile, or record one from this project.")
                .size(12)
                .color(FG_MUTED),
        ]
        .spacing(6)
        .padding(PAD * 2)
        .width(Fill);

        // An error from the last attempt outranks the general blurb.
        if let Some(e) = &self.flame_error {
            col = col.push(text(e.clone()).size(11).color(BAD));
        }

        // Why Record is unavailable, when it is — with the fix, not just the fact.
        let missing = if self.project_kind != sc_win::project::ProjectKind::Cargo {
            Some(
                tool::Missing::NotCargo {
                    kind: self.project_kind.label(),
                }
                .reason(),
            )
        } else if self.flame_tool.is_none() {
            Some(tool::Missing::NoProfiler.reason())
        } else {
            None
        };

        if let Some(why) = missing {
            col = col.push(
                container(
                    column![
                        text(why).size(11).color(FG_MUTED),
                        row![
                            button(text("Copy install command").size(11))
                                .on_press(Message::CopyInstallHint)
                                .padding([2, 8])
                                .style(menu_item_style),
                            // The panel probes at boot, so a tool installed since then is
                            // invisible until asked about. Without this the only cure for a
                            // stale answer is a restart nobody would guess at.
                            button(text("Check again").size(11))
                                .on_press(Message::RecheckProfiler)
                                .padding([2, 8])
                                .style(menu_item_style),
                        ]
                        .spacing(4),
                    ]
                    .spacing(6),
                )
                .padding(PAD)
                .style(|_t: &Theme| container::Style {
                    background: Some(Background::Color(SURFACE)),
                    border: Border {
                        color: CARD_BORDER,
                        width: 1.0,
                        ..Border::default()
                    },
                    ..container::Style::default()
                }),
            );
        } else if let Some(t) = self.flame_tool {
            col = col.push(
                text(format!("Ready to record with {}.", t.label()))
                    .size(11)
                    .color(GOOD),
            );
        }

        col = col.push(
            text(
                "Folded stacks look like:  main;run_agent;model_call 58\n\
                 Produced by cargo flamegraph --print-folded, samply, or \
                 perf script | stackcollapse-perf.pl",
            )
            .size(10)
            .color(FG_MUTED),
        );

        container(col).width(Fill).height(Fill).into()
    }
}

/// The next target in the cycle, for the toolbar's click-to-change button.
///
/// A three-state button rather than a dropdown: this crate has no `pick_list` idiom, and three
/// options do not earn a menu. Named targets (a specific `--bin` or `--bench`) are reachable by
/// typing, not here — the cycle covers the common cases.
fn next_target(t: &tool::Target) -> tool::Target {
    match t {
        tool::Target::Bin(_) => tool::Target::Bench(String::new()),
        tool::Target::Bench(_) => tool::Target::Test(None),
        tool::Target::Test(_) => tool::Target::Bin(None),
    }
}
