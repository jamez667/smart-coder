//! App view: comment box, bottom strip tabs, menus, badges.

use super::*;
use iced::widget::{column, row};

impl App {
    /// The inline comment box shown under the selected range (PR-style): a text input +
    /// Comment / Cancel. Submitting triages small (fix now) vs. big (plan first).
    pub(crate) fn view_comment_box(&self) -> Element<'_, Message> {
        let placeholder = match self.comment_range {
            Some((lo, hi)) if lo != hi => format!("comment on lines {lo}–{hi}…"),
            _ => "comment on this line…".to_string(),
        };
        let input = text_input(&placeholder, &self.comment_draft)
            .on_input(Message::CommentDraftChanged)
            .on_submit(Message::CommentSubmit)
            .padding(8)
            .style(input_style)
            .width(Fill);
        let submit = button(text("Comment").size(13))
            .on_press(Message::CommentSubmit)
            .padding([4, 12])
            .style(primary_button);
        let cancel = button(text("Cancel").size(13))
            .on_press(Message::CommentCancel)
            .padding([4, 12])
            .style(menu_item_style);
        let hint = text("small fix → done inline · bigger → we'll plan it")
            .size(10)
            .color(FG_MUTED);
        container(column![input, row![submit, cancel].spacing(6), hint].spacing(6))
            .width(Fill)
            .padding(8)
            .style(dropdown_style)
            .into()
    }

    /// The bottom panel — a tabbed strip of **Verification** (verify output) and **Build**
    /// (last run outcome). It auto-hides when there's nothing to show — no run in flight, no
    /// verify output, no build result — so planning gets the full height. During a swarm
    /// build the coder prompt/output panel takes the whole strip.
    pub(crate) fn view_bottom_strip(&self) -> Option<Element<'_, Message>> {
        if self.is_swarm() {
            return Some(self.view_coder_io());
        }
        // Show the strip when there's build/verify content OR a project is open (so the
        // integrated Terminal is always available while working in a project).
        let has_content = self.session.is_some()
            || self.verify_text.is_some()
            || self.result.is_some()
            || self.picked_workspace.is_some();
        if !has_content {
            return None;
        }
        let tabs = row![
            self.bottom_tab_button("Verification", BottomTab::Verification),
            self.bottom_tab_button("Build", BottomTab::Build),
            self.bottom_tab_button("Terminal", BottomTab::Terminal),
        ]
        .spacing(4);
        let content = match self.bottom_tab {
            BottomTab::Verification => self.view_verification_tab(),
            BottomTab::Build => self.view_build_tab(),
            BottomTab::Terminal => self.view_terminal_tab(),
        };
        Some(
            container(column![tabs, content].spacing(6))
                .width(Fill)
                .height(Length::Fixed(180.0))
                .padding(10)
                .style(card_style)
                .into(),
        )
    }

    /// One bottom-tab button; highlighted when it's the selected tab.
    pub(crate) fn bottom_tab_button(&self, label: &str, tab: BottomTab) -> Element<'_, Message> {
        let selected = self.bottom_tab == tab;
        button(
            text(label.to_string())
                .size(12)
                .color(if selected { ACCENT } else { FG_MUTED }),
        )
        .on_press(Message::SelectBottomTab(tab))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(selected, status))
        .into()
    }

    /// The Verification tab: the verify command's captured output (failure-first).
    pub(crate) fn view_verification_tab(&self) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match &self.verify_text {
            Some(v) => text(v.clone()).size(12).into(),
            None => text("cargo check / test output shows here after the agent verifies")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };
        scrollable(inner).height(Fill).into()
    }

    /// The Build tab: the last run's outcome — ✓/✗ headline, reason, changed/built files,
    /// and (from-scratch only) an open-folder button.
    pub(crate) fn view_build_tab(&self) -> Element<'_, Message> {
        let Some(r) = self.result.as_ref() else {
            return text("no build yet — describe a change and run it")
                .size(12)
                .color(FG_MUTED)
                .into();
        };
        let (mark, color) = if r.ok { ("✓", GOOD) } else { ("✗", BAD) };
        let mut col = column![text(format!("{mark}  {}", r.headline))
            .size(15)
            .color(color)]
        .spacing(4);
        if !r.reason.is_empty() {
            col = col.push(text(&r.reason).size(12).color(FG_MUTED));
        }
        let label = if self.iterating { "changed" } else { "built" };
        if !r.files.is_empty() {
            col = col.push(text(format!("files {label}:")).size(11).color(FG_MUTED));
        }
        for f in r.files.iter().take(10) {
            col = col.push(text(format!("  • {f}")).size(12));
        }
        if r.files.len() > 10 {
            col = col.push(
                text(format!("  … and {} more", r.files.len() - 10))
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        if r.dir.is_some() {
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            col = col.push(
                button(text("📂 open output folder"))
                    .on_press(Message::OpenOutputFolder)
                    .style(menu_item_style),
            );
        }
        // A finished Breakdown: make the next step unmissable. Breakdown is design-only, so offer
        // the follow-ons right here — build the plan, or commit it to the repo — instead of leaving
        // the user with a passive "plan ready" line and no obvious action.
        if r.plan_ready {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(
                text("Review the breakdown above, then:")
                    .size(12)
                    .color(FG_MUTED),
            );
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            let mut build = button(text("⚒  Build this plan").size(14).color(FG)).padding([6, 16]);
            // Only enable Build when we still have the plan task and no run is in flight.
            if self.last_plan_task.is_some() && self.session.is_none() {
                build = build.on_press(Message::BuildLastPlan).style(primary_button);
            } else {
                build = build.style(stage_toggle_button);
            }
            let commit = button(text("✓  Commit the plan").size(14).color(FG))
                .on_press(Message::CommitPlan)
                .padding([6, 16])
                .style(stage_toggle_button);
            col = col.push(row![build, commit].spacing(8));
        }
        // "I don't like this change" — undo an in-place fix's edits (git-revert its files).
        // Only for iterate runs that changed files (from-scratch builds have no committed base).
        if self.iterating && !r.files.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(6.0)));
            col = col.push(
                button(text("↩ Undo this change").size(13))
                    .on_press(Message::UndoLastChange)
                    .padding([4, 12])
                    .style(|_t: &Theme, status| {
                        let hov = matches!(status, button::Status::Hovered);
                        button::Style {
                            background: Some(Background::Color(Color {
                                a: if hov { 0.22 } else { 0.14 },
                                ..BAD
                            })),
                            text_color: FG,
                            border: Border {
                                radius: RADIUS.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    }),
            );
        }
        scrollable(col).height(Fill).into()
    }

    /// The Terminal tab: a VS-Code-style command runner. Scrollback (stderr in red, the
    /// `$ cmd`/`[exit]` meta lines dimmed) above an input row; Run becomes Kill while a
    /// command is in flight. Commands run in the open workspace via [`sc_win::terminal`].
    /// A persistent one-line badge describing where terminal commands run right now, so
    /// containment is never ambiguous: `(text, colour)`.
    pub(crate) fn term_status_badge(&self) -> (String, iced::Color) {
        // `cfg.sandbox()` only yields Host/Docker; Session is a runtime-only state, folded in
        // here for exhaustiveness (rendered like Docker).
        let image = match self.cfg.sandbox() {
            sc_verify::Sandbox::Host => {
                return (
                    "⚠ HOST — commands run on this machine (sandbox off)".to_string(),
                    BAD,
                );
            }
            sc_verify::Sandbox::Docker { image } => image,
            sc_verify::Sandbox::Session(c) => c.name().to_string(),
        };
        if self.picked_workspace.is_none() {
            (
                "🔒 sandbox on — open a project to enable the terminal".to_string(),
                FG_MUTED,
            )
        } else if self.term_container_started {
            (
                format!("🔒 sandboxed — running in container ({image})"),
                GOOD,
            )
        } else {
            (
                format!("🔒 sandboxed — container starts on first command ({image})"),
                FG_MUTED,
            )
        }
    }

    pub(crate) fn view_terminal_tab(&self) -> Element<'_, Message> {
        use sc_win::terminal::Stream;
        // Persistent containment badge — always visible so you know where commands execute.
        let (badge_text, badge_color) = self.term_status_badge();
        let badge = text(badge_text).size(11).color(badge_color);
        // Scrollback: one monospace line per output line, coloured by originating stream.
        let mut col = column![].spacing(0);
        if self.terminal.lines.is_empty() {
            col = col.push(
                text("type a command and press Enter — e.g.  cargo build -p sc-win")
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        for line in &self.terminal.lines {
            let color = match line.stream {
                Stream::Stdout => FG,
                Stream::Stderr => BAD,
                Stream::Meta => FG_MUTED,
            };
            col = col.push(
                text(line.text.clone())
                    .size(12)
                    .font(iced::Font::MONOSPACE)
                    .color(color),
            );
        }
        let scrollback = scrollable(col).height(Fill).anchor_bottom().width(Fill);

        // Input row: prompt glyph · input box · Run/Kill · Clear.
        let running = self.terminal.running;
        let input = text_input("command…", &self.terminal.input)
            .on_input(Message::TermInput)
            .on_submit(Message::TermSubmit)
            .padding([6, 8])
            .font(iced::Font::MONOSPACE)
            .style(input_style_borderless)
            .width(Fill);
        let action = if running {
            button(text("⏹ Kill").size(13))
                .on_press(Message::TermKill)
                .padding([4, 12])
                .style(menu_item_style)
        } else {
            button(text("▶ Run").size(13))
                .on_press(Message::TermSubmit)
                .padding([4, 12])
                .style(menu_item_style)
        };
        let clear = button(text("Clear").size(13).color(FG_MUTED))
            .on_press(Message::TermClear)
            .padding([4, 12])
            .style(menu_item_style);
        // Command-history recall (↑ previous, ↓ next) — the arrow-key affordance as buttons,
        // since `text_input` doesn't surface arrow keys.
        let hist_prev = button(text("↑").size(13).font(iced::Font::MONOSPACE))
            .on_press(Message::TermHistoryPrev)
            .padding([4, 8])
            .style(menu_item_style);
        let hist_next = button(text("↓").size(13).font(iced::Font::MONOSPACE))
            .on_press(Message::TermHistoryNext)
            .padding([4, 8])
            .style(menu_item_style);
        let prompt = text(if running { "…" } else { "$" })
            .size(13)
            .font(iced::Font::MONOSPACE)
            .color(if running { ACCENT } else { GOOD });
        let input_row = row![prompt, input, hist_prev, hist_next, action, clear]
            .spacing(6)
            .align_y(iced::Alignment::Center);

        column![badge, scrollback, input_row]
            .spacing(6)
            .height(Fill)
            .into()
    }

    /// The horizontal step-flow at the top: each phase with arrows between, the current
    /// phase highlighted, done phases checked, plus a final "Build" step that lights up
    /// once planning is complete and implementation begins.
    pub(crate) fn view_step_flow(&self) -> Element<'_, Message> {
        let current = self.plan.current_phase();
        let done_color = iced::Color::from_rgb(0.45, 0.78, 0.55); // green
        let now_color = iced::Color::from_rgb(0.48, 0.65, 0.98); // blue
        let dim_color = iced::Color::from_rgb(0.4, 0.43, 0.55);
        let arrow_color = iced::Color::from_rgb(0.35, 0.38, 0.5);

        let mut flow = row![].spacing(6).align_y(iced::Alignment::Center);
        let steps = self.plan.steps();
        for (i, step) in steps.iter().enumerate() {
            let is_current = current == Some(step.phase);
            let (mark, color, size) = if step.done {
                ("✓", done_color, 13)
            } else if is_current {
                ("▶", now_color, 15)
            } else {
                ("·", dim_color, 13)
            };
            let label = text(format!("{mark} {}", step.phase.title()))
                .size(size)
                .color(color);
            flow = flow.push(label);
            flow = flow.push(text("→").size(13).color(arrow_color));
            let _ = i;
        }
        // The final "Build" step: active once planning is complete (no current phase
        // left) and a swarm is running; done when source files were built.
        let built = self.result.as_ref().is_some_and(|r| r.ok);
        let building = current.is_none() && self.is_swarm();
        let (bmark, bcolor) = if built {
            ("✓", done_color)
        } else if building {
            ("▶", now_color)
        } else {
            ("·", dim_color)
        };
        flow = flow.push(
            text(format!("{bmark} Build"))
                .size(if building { 15 } else { 13 })
                .color(bcolor),
        );

        container(
            scrollable(flow).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            )),
        )
        .width(Fill)
        .padding(10)
        .style(card_style)
        .into()
    }

    /// The top menu bar: the app title, the dropdown menu buttons (File / View), and —
    /// pushed to the right — the workspace/status line (what folder we're working in, and
    /// how). Clicking a title toggles its dropdown; items live in [`Self::view_menu_dropdown`].
    pub(crate) fn view_menu_bar(&self) -> Element<'_, Message> {
        // No brand logo here — the window/taskbar icon already shows it right above.
        let file = self.menu_title("File", Menu::File);
        let view_m = self.menu_title("View", Menu::View);
        // Layout: File/View at the left · workspace status centered · model health badge
        // at the right. Two flexible spacers center the status between the fixed ends.
        let status = text(self.workspace_status())
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70));
        let health = self.view_backend_badge();
        let bar = row![
            file,
            view_m,
            Space::new().width(Fill), // left spacer
            status,
            Space::new().width(Fill), // right spacer → status sits centered
            health,
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        container(bar)
            .width(Fill)
            .padding([4, 10])
            .style(menu_bar_style)
            .into()
    }

    /// A human reason the backend is known-unusable, or `None` if it's `Ready` or not yet
    /// probed. Used to preflight-gate runs so a bad backend fails loudly up front instead of
    /// mid-stream. A `None` (unprobed) health is allowed through — we don't block the first
    /// few seconds after launch before the first probe lands.
    pub(crate) fn backend_unready_reason(&self) -> Option<String> {
        use sc_model::BackendHealth::*;
        match &self.backend_health {
            Some(NoModel { .. }) => Some("backend is up but no model is loaded".to_string()),
            Some(Unreachable { .. }) => {
                Some(format!("backend unreachable at {}", self.cfg.base_url))
            }
            None | Some(Ready) => None,
        }
    }

    /// The backend health badge in the top bar (after File/View): a coloured dot + short
    /// label from the periodic probe. Green = a real completion succeeded (model serving);
    /// amber = endpoint reachable but no model loaded; red = unreachable; grey = probing.
    pub(crate) fn view_backend_badge(&self) -> Element<'_, Message> {
        use sc_model::BackendHealth::*;
        let (dot, label, color) = match &self.backend_health {
            None => ("●", "checking backend…".to_string(), FG_MUTED),
            Some(Ready) => ("●", format!("{} ready", self.cfg.model), GOOD),
            Some(NoModel { .. }) => (
                "●",
                "backend up — no model loaded".to_string(),
                Color::from_rgb(0.95, 0.72, 0.30), // amber
            ),
            Some(Unreachable { .. }) => (
                "●",
                format!("backend unreachable ({})", self.cfg.base_url),
                BAD,
            ),
        };
        row![
            text(dot).size(12).color(color),
            text(label).size(11).color(color),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center)
        .into()
    }

    /// The one-line workspace status shown at the right of the top bar: where the app is
    /// working and in which mode.
    pub(crate) fn workspace_status(&self) -> String {
        match (&self.picked_workspace, &self.run_dir) {
            (Some(dir), _) => {
                let stack = sc_workflow::ProjectStack::detect(dir).label();
                format!("iterating in  {}  ·  {stack}", dir.display())
            }
            (None, Some(d)) => format!("output  {}", d.display()),
            (None, None) => "no project — File ▸ Open folder".to_string(),
        }
    }

    /// One clickable menu-bar title; highlighted while its dropdown is open.
    pub(crate) fn menu_title(&self, label: &str, which: Menu) -> Element<'_, Message> {
        let open = self.open_menu == Some(which);
        button(
            text(label.to_string())
                .size(13)
                .color(if open { ACCENT } else { FG }),
        )
        .on_press(Message::ToggleMenu(which))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(open, status))
        .into()
    }

    /// The open dropdown's items, rendered as a floating Windows-style card positioned
    /// directly under its menu-bar title (via top/left spacers in an overlay layer, so it
    /// never shifts the base layout). A full-window transparent backdrop behind it closes
    /// the menu on an outside click. Returns `None` when no menu is open.
    pub(crate) fn view_menu_dropdown(&self) -> Option<Element<'_, Message>> {
        let which = self.open_menu?;
        let items: Vec<(String, Message)> = match which {
            Menu::File => {
                let mut v: Vec<(String, Message)> = if self.picked_workspace.is_some() {
                    vec![
                        (
                            "📁  Open a different folder…".to_string(),
                            Message::PickWorkspace,
                        ),
                        ("✕  Close project".to_string(), Message::ClearWorkspace),
                    ]
                } else {
                    vec![("📁  Open folder…".to_string(), Message::PickWorkspace)]
                };
                // Recent projects (most-recent first), excluding the currently-open one.
                let recents = sc_win::persist::load().recents;
                let current = self.picked_workspace.clone();
                let recent_items: Vec<_> = recents
                    .into_iter()
                    .filter(|p| Some(p) != current.as_ref())
                    .take(8)
                    .collect();
                if !recent_items.is_empty() {
                    v.push(("— Recent —".to_string(), Message::NoOp));
                    for p in recent_items {
                        let name = p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("(project)")
                            .to_string();
                        v.push((format!("🕘  {name}"), Message::OpenRecent(p)));
                    }
                }
                v
            }
            Menu::View => vec![(
                if self.settings_open {
                    "⚙  Hide settings".to_string()
                } else {
                    "⚙  Settings…".to_string()
                },
                Message::ToggleSettings,
            )],
        };
        let mut col = column![].spacing(0);
        for (label, msg) in items {
            // A "— … —" entry is a non-clickable section header, rendered as dimmed text.
            if label.starts_with("— ") {
                col = col.push(
                    container(text(label).size(11).color(FG_MUTED))
                        .padding([6, 14])
                        .width(Length::Fixed(230.0)),
                );
            } else {
                col = col.push(
                    button(text(label).size(13).color(FG))
                        .on_press(msg)
                        .padding([6, 14])
                        .width(Length::Fixed(230.0))
                        .style(menu_item_style),
                );
            }
        }
        let card = container(col).padding(3).style(dropdown_style);

        // Position under the right title. With the brand logo removed, File now starts right
        // after the menu bar's 10px left padding; View follows it. Spacers place the card; a
        // transparent full-window backdrop (a mouse_area) closes the menu on any outside click.
        let left = match which {
            Menu::File => 8.0,
            Menu::View => 48.0,
        };
        let positioned = column![
            Space::new().height(Length::Fixed(30.0)),
            row![Space::new().width(Length::Fixed(left)), card],
        ];
        // Backdrop: an invisible full-size mouse_area that closes the menu when clicked.
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleMenu(which)); // re-toggle closes it

        Some(
            iced::widget::stack![backdrop, positioned]
                .width(Fill)
                .height(Fill)
                .into(),
        )
    }

    /// The Settings modal: a centered card floating over a dimmed backdrop (an overlay
    /// layer in `view`'s Stack). Clicking the backdrop or the ✕ closes it. This replaces the
    /// old inline panel that pushed the layout around.
    pub(crate) fn view_settings_modal(&self) -> Element<'_, Message> {
        // The dim backdrop; clicking it closes settings.
        let backdrop =
            iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill).style(
                |_t: &Theme| container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.55,
                        ..Color::BLACK
                    })),
                    ..container::Style::default()
                },
            ))
            .on_press(Message::ToggleSettings);

        // The settings card itself, centered, with a close button in its header.
        let header = row![
            text("Settings").size(16).color(FG),
            Space::new().width(Fill),
            button(text("✕").size(14))
                .on_press(Message::ToggleSettings)
                .padding([2, 8])
                .style(menu_item_style),
        ]
        .align_y(iced::Alignment::Center);
        let card = container(column![header, self.view_settings_body()].spacing(12))
            .width(Length::Fixed(520.0))
            .max_width(560.0)
            .padding(18)
            .style(dropdown_style);

        // `opaque` stops clicks on the card from falling through to the backdrop.
        iced::widget::stack![backdrop, iced::widget::center(iced::widget::opaque(card))]
            .width(Fill)
            .height(Fill)
            .into()
    }
}
