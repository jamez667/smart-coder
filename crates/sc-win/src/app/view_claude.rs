//! The **Claude Code** panel (spec 22): its own task input, its own run feed.
//!
//! A panel rather than a button on the chat composer. Claude Code is a peer of the agent, not
//! a mode of it — and a surface with its own input needs its own output beside it. The first
//! attempt hung a button off the Chat composer, which buried it inside an Assistant-only panel
//! AND left the run with nowhere to appear: it wrote to the activity stream, which the chat
//! panel does not render. A run that works and shows nothing is indistinguishable from one that
//! never started, which is exactly how it presented.

use super::*;
use iced::widget::{column, row, scrollable};

impl App {
    /// The Claude Code panel: feed on top, input at the bottom.
    pub(crate) fn view_claude_panel(&self) -> Element<'_, Message> {
        let running = self.session.is_some() && self.claude_run;

        // --- The feed -------------------------------------------------------------------
        let body: Element<'_, Message> = if self.claude_feed.is_empty() {
            // An empty panel should say what it is for, not sit blank. Two different empties:
            // before anything has run, and while the FIRST events are still arriving.
            let msg = if running {
                "Working…"
            } else {
                "Describe a task below and press Run.\nClaude Code works in this project folder."
            };
            container(text(msg).size(12).color(FG_MUTED))
                .width(Fill)
                .height(Fill)
                .padding(PAD)
                .into()
        } else {
            let mut col = column![].spacing(3).padding(PAD).width(Fill);
            for r in &self.claude_feed {
                // Tool-call lines and errors are single short strings and read
                // better as they are — icon, one line, done. Only the model's PROSE
                // (`💬`) and replayed conversation carry Markdown worth rendering,
                // and a page of it as flat size-12 text is the wall this fixes.
                if r.is_error || !matches!(r.icon, "💬" | "·" | "❯") || r.text.is_empty() {
                    col = col.push(
                        row![
                            text(r.icon)
                                .size(12)
                                .color(if r.is_error { BAD } else { FG_MUTED }),
                            text(&r.text)
                                .size(12)
                                .color(if r.is_error { BAD } else { FG }),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Start),
                    );
                    continue;
                }
                col = col.push(
                    row![
                        text(r.icon).size(12).color(FG_MUTED),
                        markdown_body(&r.text),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Start),
                );
            }
            scrollable(col)
                .id(claude_feed_id())
                .width(Fill)
                .height(Fill)
                .into()
        };

        // --- The delegated-approvals notice ---------------------------------------------
        // Beside the run it describes (spec 22). A notice in the gate bar would be useless to
        // anyone who does not have that bar on screen, and silence during a run that is editing
        // files reads as "nothing needed approving" — a lie by omission.
        let mut stack = column![body].width(Fill).height(Fill);
        if running {
            stack = stack.push(
                container(
                    text("Claude Code is handling its own permission prompts for this run.")
                        .size(11)
                        .color(FG_MUTED),
                )
                .width(Fill)
                .padding([4, PAD]),
            );
        }

        // --- The input ------------------------------------------------------------------
        let input = text_input("Ask Claude Code to do something…", &self.claude_input)
            .on_input(Message::ClaudeInputChanged)
            .on_submit(Message::RunClaudeCode)
            .padding([8, 10])
            .style(input_style_borderless)
            .width(Fill);
        // Run while idle; Stop while working. One button, because they are the same affordance
        // at different times — and a Stop that is always visible but usually dead is noise.
        let btn = if running {
            button(text("⏹ Stop").size(14).width(Fill).height(Fill).center())
                .on_press(Message::CancelRun)
                .width(Length::Fixed(84.0))
                .height(Fill)
                .padding(0)
                .style(menu_item_style)
        } else {
            button(text("✦ Run").size(14).width(Fill).height(Fill).center())
                .on_press(Message::RunClaudeCode)
                .width(Length::Fixed(84.0))
                .height(Fill)
                .padding(0)
                .style(primary_button)
        };
        // The ⚙ opens the options menu. A button rather than a permanent strip: the option set
        // grows (the CLI has a dozen flags worth exposing) and a strip does not scale past
        // about four before it eats the panel.
        let gear = button(text("⚙").size(14).width(Fill).height(Fill).center())
            .on_press(Message::ToggleClaudeMenu)
            .width(Length::Fixed(34.0))
            .height(Fill)
            .padding(0)
            .style(menu_item_style);

        const INPUT_H: f32 = 38.0;
        let bar = container(
            row![input, gear, btn]
                .spacing(0)
                .align_y(iced::Alignment::Center)
                .height(Length::Fixed(INPUT_H)),
        )
        .width(Fill)
        .padding([0, 0]);

        // A one-line summary of anything set away from its default, so the panel says what it
        // will do without opening the menu. Silent when everything is default — a status line
        // that always reads "Default · Ask as usual" is noise that trains you to ignore it.
        let mut chips: Vec<String> = Vec::new();
        let o = &self.cfg.claude;
        if o.model != sc_win::claudecode::Model::Default {
            chips.push(o.model.label().to_string());
        }
        if o.permission != sc_win::claudecode::Permission::Default {
            chips.push(o.permission.label().to_string());
        }
        if let Some(id) = &self.claude_session {
            chips.push(format!("resuming {}", &id[..id.len().min(8)]));
        } else if o.continue_session {
            chips.push("continuing".to_string());
        }
        if !o.add_dirs.is_empty() {
            chips.push(format!("+{} dir(s)", o.add_dirs.len()));
        }
        if !o.allowed_tools.is_empty() {
            chips.push(format!("only {}", o.allowed_tools.join(", ")));
        }
        if !o.disallowed_tools.is_empty() {
            chips.push(format!("no {}", o.disallowed_tools.join(", ")));
        }
        let mut bottom = column![].width(Fill);
        if !chips.is_empty() {
            bottom = bottom.push(
                container(text(chips.join("  ·  ")).size(10).color(FG_MUTED))
                    .width(Fill)
                    .padding([2, PAD]),
            );
        }
        bottom = bottom.push(bar);

        let base = column![stack, bottom].width(Fill).height(Fill);
        if !self.claude_menu {
            return base.into();
        }
        // The menu floats OVER the panel rather than pushing the feed around, so opening it
        // doesn't reflow what you were reading.
        iced::widget::stack![base, self.view_claude_menu()]
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// The ⚙ options menu: a filter box over grouped, filterable actions.
    ///
    /// Grouped and filterable because the option list is already at ten items and will grow —
    /// the same reason the CLI's own action list works this way. Each row states its CURRENT
    /// value on the right, so the menu doubles as the status display.
    fn view_claude_menu(&self) -> Element<'_, Message> {
        let o = &self.cfg.claude;
        let q = self.claude_filter.trim().to_lowercase();
        // A row survives the filter if the query appears in its label or its value — so typing
        // "opus" finds the model row by what it is set to, not only by its name.
        let hit = |label: &str, value: &str| {
            q.is_empty() || label.to_lowercase().contains(&q) || value.to_lowercase().contains(&q)
        };

        let mut items = column![].spacing(1).width(Fill);
        let mut any = false;
        let section = |items: &mut iced::widget::Column<'_, Message>, name: &'static str| {
            *items = std::mem::replace(items, column![])
                .push(container(text(name).size(10).color(FG_MUTED)).padding([6, 10]));
        };

        // --- Context ---------------------------------------------------------------------
        let has_file = self.panes.focused().selected_file.is_some();
        let attach_label = match self.panes.focused().selected_file.as_deref() {
            Some(f) => format!("Attach {f}"),
            None => "Attach the open file…".to_string(),
        };
        let ctx: Vec<(String, String, Option<Message>)> = vec![
            (
                attach_label,
                String::new(),
                has_file.then_some(Message::AttachActiveFile),
            ),
            (
                "Add a directory…".to_string(),
                if o.add_dirs.is_empty() {
                    String::new()
                } else {
                    format!("{}", o.add_dirs.len())
                },
                Some(Message::AddClaudeDir),
            ),
            (
                "Continue most recent conversation".to_string(),
                if o.continue_session { "on" } else { "off" }.to_string(),
                Some(Message::ToggleClaudeContinue),
            ),
            (
                "Clear this conversation".to_string(),
                String::new(),
                Some(Message::ClearClaudeRun),
            ),
        ];
        let ctx: Vec<_> = ctx.into_iter().filter(|(l, v, _)| hit(l, v)).collect();
        if !ctx.is_empty() {
            any = true;
            section(&mut items, "Context");
            for (label, value, msg) in ctx {
                items = items.push(menu_row(label, value, msg));
            }
        }

        // --- Model -----------------------------------------------------------------------
        let model_rows: Vec<(String, String, Option<Message>)> = vec![
            (
                "Switch model".to_string(),
                o.model.label().to_string(),
                Some(Message::CycleClaudeModel),
            ),
            (
                "Permissions".to_string(),
                o.permission.label().to_string(),
                Some(Message::CycleClaudePermission),
            ),
        ];
        let model_rows: Vec<_> = model_rows
            .into_iter()
            .filter(|(l, v, _)| hit(l, v))
            .collect();
        if !model_rows.is_empty() {
            any = true;
            section(&mut items, "Model");
            for (label, value, msg) in model_rows {
                items = items.push(menu_row(label, value, msg));
            }
        }

        // --- Tools -----------------------------------------------------------------------
        // Free text rather than a checklist: the CLI accepts patterns like `Bash(git *)`, and a
        // checklist of tool names could not express one.
        if hit("tools allowed disallowed", "") || !q.is_empty() {
            let allowed = o.allowed_tools.join(" ");
            let disallowed = o.disallowed_tools.join(" ");
            if hit("Allowed tools", &allowed) || hit("Disallowed tools", &disallowed) {
                any = true;
                section(&mut items, "Tools");
                items = items.push(
                    container(
                        column![
                            text("Only these tools (blank = all)")
                                .size(11)
                                .color(FG_MUTED),
                            text_input("Edit Bash(git *)", &allowed)
                                .on_input(Message::ClaudeAllowedChanged)
                                .padding([5, 8])
                                .size(12)
                                .style(input_style),
                            text("Never these").size(11).color(FG_MUTED),
                            text_input("WebFetch", &disallowed)
                                .on_input(Message::ClaudeDisallowedChanged)
                                .padding([5, 8])
                                .size(12)
                                .style(input_style),
                        ]
                        .spacing(4),
                    )
                    .padding([4, 10]),
                );
            }
        }

        // The extra directories, each removable — a list you can add to but not edit out of is
        // a trap.
        for (i, d) in o.add_dirs.iter().enumerate() {
            if !hit(d, "") {
                continue;
            }
            any = true;
            items = items.push(menu_row(
                format!("  ✕  {d}"),
                String::new(),
                Some(Message::RemoveClaudeDir(i)),
            ));
        }

        // Past conversations, newest first — the picker. Read from the CLI's own
        // session logs rather than shelling out to `--resume` with no id, which
        // opens an interactive TUI this panel has no terminal to draw.
        //
        // `workspace_root()`, NOT `cfg.workspace`: opening a project sets
        // `picked_workspace` and leaves the config at its default (a temp dir the CLI
        // has never run in), so reading the config listed sessions for the wrong
        // folder and always found none.
        let ws_root = self.workspace_root();
        let sessions = sc_win::claudesessions::list(&ws_root);
        if sessions.is_empty() {
            // SAY SO, and name the folder. Sessions are per-workspace, so a silent
            // absence reads as a broken feature rather than "nothing here yet".
            let here = ws_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| ws_root.display().to_string());
            let note = format!("No past conversations in {here}");
            if hit(&note, "") {
                any = true;
                section(&mut items, "Resume");
                items = items.push(container(text(note).size(12).color(FG_MUTED)).padding([5, 10]));
            }
        } else {
            let rows: Vec<&sc_win::claudesessions::Session> =
                sessions.iter().filter(|x| hit(&x.summary, "")).collect();
            if !rows.is_empty() {
                any = true;
                section(&mut items, "Resume a conversation");
                for sess in rows {
                    let active = self.claude_session.as_deref() == Some(sess.id.as_str());
                    // The house row shape: label left, value right-aligned. The
                    // summary is the label and the AGE is the value — with a dozen
                    // conversations the question alone does not distinguish
                    // yesterday's from last month's. No per-row icon: an `↺` on every
                    // line is noise, so only the ACTIVE one is marked.
                    let label = if active {
                        format!("● {}", sess.summary)
                    } else {
                        sess.summary.clone()
                    };
                    items = items.push(menu_row(
                        label,
                        relative_age(sess.modified),
                        Some(Message::ResumeClaudeSession(sess.id.clone())),
                    ));
                }
            }
        }

        if !any {
            items = items.push(
                container(text("No matching options").size(12).color(FG_MUTED)).padding([6, 10]),
            );
        }

        let filter = text_input("Filter options…", &self.claude_filter)
            .on_input(Message::ClaudeFilterChanged)
            .padding([6, 10])
            .size(12)
            .style(input_style_borderless)
            .width(Fill);

        let card = container(
            column![
                filter,
                scrollable(items).height(Length::Fixed(300.0)).width(Fill),
            ]
            .spacing(2),
        )
        .width(Length::Fixed(340.0))
        .padding(4)
        .style(dropdown_style);

        // Clicking anywhere off the card closes it — the convention every other menu here uses.
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleClaudeMenu);

        iced::widget::stack![
            backdrop,
            container(iced::widget::opaque(card))
                .width(Fill)
                .height(Fill)
                .align_x(iced::alignment::Horizontal::Right)
                .align_y(iced::alignment::Vertical::Bottom)
                .padding([44, PAD]),
        ]
        .width(Fill)
        .height(Fill)
        .into()
    }
}

/// One row of the options menu: a label, its current value on the right, and an action.
///
/// `None` for the message renders it inert rather than hiding it — "Attach the open file" with
/// no file open should say what it would do, not vanish and leave you wondering where it went.
/// "3h" / "2d" / "5w" — how long ago, in the least space that still says it.
///
/// A wall-clock date would be more precise and less useful: the question a picker
/// answers is "which of these is the one I was just in", and an age answers it at a
/// glance where `2026-08-28 08:14` has to be read and compared.
fn relative_age(modified_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(modified_secs);
    let secs = now.saturating_sub(modified_secs);
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=604_799 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

/// Render Markdown prose as a styled column: headings, bullets, code and gaps.
///
/// The panel used to push a whole answer through as one flat line of size-12 text,
/// which is why a page of it read as an undifferentiated wall next to any other
/// chat client. Only the structure a coding model actually emits is handled — see
/// `sc_win::markdown` for why the parser stops there.
fn markdown_body(src: &str) -> Element<'static, Message> {
    use sc_win::markdown::{parse, Block};

    let mut col = column![].spacing(2).width(Fill);
    for block in parse(src) {
        col = col.push(match block {
            // A gap, not an empty row: paragraph breaks are what let the eye find
            // the start of a thought.
            Block::Blank => Space::new().height(Length::Fixed(6.0)).into(),
            Block::Heading { level, spans } => {
                // Bigger and brighter, and the accent colour on the top level so a
                // long answer has landmarks you can scan for.
                let size = match level {
                    1 => 15.0,
                    2 => 14.0,
                    _ => 13.0,
                };
                container(inline(spans, size, if level == 1 { ACCENT } else { FG }))
                    .padding([4, 0])
                    .into()
            }
            Block::Bullet { spans } => {
                row![text("•").size(12).color(FG_MUTED), inline(spans, 12.0, FG),]
                    .spacing(6)
                    .align_y(iced::Alignment::Start)
                    .into()
            }
            // Monospace on the editor background, so a block reads as code at a
            // glance rather than as oddly-worded prose.
            Block::Code(line) => {
                container(text(line).size(11).font(iced::Font::MONOSPACE).color(GOOD))
                    .width(Fill)
                    .padding([1, 8])
                    .style(|_: &iced::Theme| container::Style {
                        background: Some(EDITOR_BG.into()),
                        ..Default::default()
                    })
                    .into()
            }
            Block::Para { spans } => inline(spans, 12.0, FG),
        });
    }
    col.into()
}

/// One line's spans as a wrapping row: plain, **bold**, and `code`.
///
/// A `row` rather than a single `text`, because iced styles a whole `text` widget
/// at once — mixed emphasis inside one line needs one widget per run.
fn inline(spans: Vec<sc_win::markdown::Span>, size: f32, fg: Color) -> Element<'static, Message> {
    use sc_win::markdown::Span;

    let mut r = row![].spacing(0).align_y(iced::Alignment::Center);
    for sp in spans {
        r = r.push(match sp {
            Span::Plain(t) => text(t).size(size).color(fg),
            // Brighter rather than a bold face: the UI ships one weight, so weight
            // is not available and contrast is what reads as emphasis.
            Span::Bold(t) => text(t).size(size).color(Color::WHITE),
            Span::Code(t) => text(t)
                .size(size - 1.0)
                .font(iced::Font::MONOSPACE)
                .color(GOOD),
        });
    }
    //  so a long line breaks at the panel edge instead of running past it.
    r.wrap().into()
}

fn menu_row(label: String, value: String, msg: Option<Message>) -> Element<'static, Message> {
    let inner = row![
        text(label)
            .size(12)
            .color(if msg.is_some() { FG } else { FG_MUTED }),
        Space::new().width(Fill),
        text(value).size(11).color(FG_MUTED),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    match msg {
        Some(m) => button(inner)
            .on_press(m)
            .width(Fill)
            .padding([5, 10])
            .style(menu_item_style)
            .into(),
        None => container(inner).width(Fill).padding([5, 10]).into(),
    }
}
