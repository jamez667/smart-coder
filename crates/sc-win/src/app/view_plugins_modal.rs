//! The plugin manager (spec 25): what loaded, what did not, why, and the switch.
//!
//! # A modal, not a bottom-strip tab
//!
//! It started as a tab beside Problems and Terminal, and that was wrong twice over. The
//! bottom strip is for per-PROJECT work — what failed to compile, what is running in the
//! terminal — and which plugins are installed is not that. Worse, the strip only renders
//! when a project is open, so the panel that explains why a plugin did not load was
//! unreachable in exactly the state where someone would go looking for it.
//!
//! # Why this is a prerequisite rather than a follow-up
//!
//! The host's whole failure story is "nothing a plugin does takes the editor with it" —
//! a missing program, a crash, a hang at handshake, a stream of unparseable lines are
//! each recorded and survived. That is only worth anything if it is *visible*. A plugin
//! that silently does not appear is indistinguishable from one that was never installed,
//! and the user's next move in that case is to reinstall a plugin that was working fine.
//!
//! So this panel is the other half of every `Failure` variant, and it is built in rather
//! than being a plugin itself — a plugin that reports plugin failures cannot report its
//! own.
//!
//! # What it shows
//!
//! Per plugin: whether it is running, how long its handshake took, what it contributes,
//! how many lines the host could not parse, and the tail of its log. Per failure: the
//! sentence from [`Failure::describe`], which is why that method refuses to return
//! "plugin failed to load".
//!
//! Two whole-set problems get their own lines, because neither belongs to any one
//! plugin: command ids two plugins both claimed, and panels dropped from the saved layout
//! because their plugin is absent. The second is the "silently halves the feed" failure
//! the Claude driver counts skipped lines to avoid — a panel vanishing without
//! explanation is a mystery, and one sentence turns it into a fact.
//!
//! # The switch says "restart", and means it
//!
//! Toggling writes `enabled` into the plugin's `plugin.json` and changes nothing else.
//! Plugins load at startup and there is no hot-swap (spec 25), so the modal says a
//! restart is needed rather than pretending the change took. Claiming otherwise would be
//! the one lie a plugin manager cannot afford: the user would toggle, see nothing happen,
//! and conclude the toggle is broken.

use super::*;
use iced::widget::{column, row};

use sc_craft_ui::plugin::Failure;

impl App {
    /// The plugin manager, as a modal over the whole window.
    pub(crate) fn view_plugins_modal(&self) -> Element<'_, Message> {
        // The dim backdrop; clicking it closes, like every other modal here.
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
            .on_press(Message::TogglePluginsModal);

        let header = row![
            text("Plugins").size(16).color(FG),
            Space::new().width(Fill),
            button(text("✕").size(14))
                .on_press(Message::TogglePluginsModal)
                .padding([2, 8])
                .style(menu_item_style),
        ]
        .align_y(iced::Alignment::Center);

        let card = container(column![header, self.view_plugins_body()].spacing(12))
            .width(Length::Fixed(560.0))
            .max_width(600.0)
            .padding(18)
            .style(dropdown_style);

        // `opaque` stops clicks on the card falling through to the backdrop.
        iced::widget::stack![
            backdrop,
            iced::widget::opaque(
                container(card)
                    .width(Fill)
                    .height(Fill)
                    .align_x(iced::alignment::Horizontal::Center)
                    .align_y(iced::alignment::Vertical::Center)
            )
        ]
        .into()
    }

    /// The manager's contents.
    fn view_plugins_body(&self) -> Element<'_, Message> {
        let Some(plugins) = self.plugins.as_ref() else {
            // Distinct from "no plugins installed": the host never ran. Only reachable in
            // a test-constructed App, but saying so beats an empty panel.
            return container(
                text("The plugin host is not running.")
                    .size(12)
                    .color(FG_MUTED),
            )
            .padding(10)
            .width(Fill)
            .into();
        };

        let mut col = column![].spacing(10).padding(10);

        // The directory, always — it is the answer to "where do I put one?", and it is
        // also the answer to "why is my plugin not listed?".
        col = col.push(
            text(format!(
                "Plugins load from {}",
                sc_craft_ui::plugin::plugins_dir().display()
            ))
            .size(11)
            .color(FG_MUTED),
        );

        if plugins.running.is_empty() && plugins.failed.is_empty() {
            col = col.push(text("No plugins installed.").size(12).color(FG_MUTED));
            return container(col).width(Fill).into();
        }

        // Whole-set problems first: they explain symptoms the per-plugin rows cannot.
        if plugins.dropped_layout_panels > 0 {
            let n = plugins.dropped_layout_panels;
            col = col.push(
                text(format!(
                    "{n} panel{} from plugins that are not loaded {} removed from your layout.",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "was" } else { "were" },
                ))
                .size(11)
                .color(AMBER),
            );
        }
        for id in &plugins.command_collisions {
            col = col.push(
                text(format!(
                    "Two plugins claim the command “{id}”; the second was dropped."
                ))
                .size(11)
                .color(AMBER),
            );
        }

        for p in &plugins.running {
            col = col.push(self.view_running_plugin(p));
        }
        for (name, failure) in &plugins.failed {
            col = col.push(view_failed_plugin(name, failure));
        }

        // A failed write is reported where the click happened. Silently failing to
        // disable a plugin would leave the user certain they had turned it off.
        if let Some(err) = &self.plugin_toggle_error {
            col = col.push(text(err).size(11).color(BAD));
        }

        // Only after a toggle, because a restart notice on every open is noise that
        // teaches people to ignore it.
        if self.plugins_need_restart {
            col = col.push(
                text("Restart Smart Coder to apply. Plugins load at startup.")
                    .size(11)
                    .color(AMBER),
            );
        }

        // Bounded rather than `Fill`: this is a card in the middle of the window, not a
        // panel, so it grows with its content up to a height that still leaves the
        // backdrop visible.
        container(scrollable(col).height(Length::Fixed(420.0)))
            .width(Fill)
            .into()
    }

    /// One running plugin.
    fn view_running_plugin<'a>(
        &'a self,
        p: &'a sc_craft_ui::plugin::Plugin,
    ) -> Element<'a, Message> {
        // The manifest's name once the handshake landed, the directory name before it —
        // so a plugin is identifiable at every stage rather than blank until it speaks.
        let name = p
            .manifest
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| p.dir_name.clone());
        let version = p
            .manifest
            .as_ref()
            .map(|m| m.version.clone())
            .filter(|v| !v.is_empty());

        let mut header = row![text(name).size(12).color(FG)].spacing(8);
        if let Some(v) = version {
            header = header.push(text(v).size(11).color(FG_MUTED));
        }
        header = header.push(Space::new().width(Fill));
        header = header.push(text("running").size(11).color(GOOD));
        // Disabling takes effect at the next launch, so the button says "Disable" rather
        // than showing a state that has not changed yet.
        header = header.push(
            button(text("Disable").size(11))
                .on_press(Message::SetPluginEnabled(p.dir_name.clone(), false))
                .padding([2, 8])
                .style(menu_item_style),
        );

        let mut body = column![header].spacing(4);

        // What it contributes, counted rather than listed: the list is visible in the
        // View menu and the command palette, and repeating it here would be the longest
        // part of a panel whose job is diagnosis.
        if let Some(m) = p.manifest.as_ref() {
            body = body.push(
                text(format!(
                    "{} panel{} · {} command{}",
                    m.panels.len(),
                    if m.panels.len() == 1 { "" } else { "s" },
                    m.commands.len(),
                    if m.commands.len() == 1 { "" } else { "s" },
                ))
                .size(11)
                .color(FG_MUTED),
            );
        }

        // Handshake time: the part of startup a plugin can make slow, and the only way to
        // tell which one is doing it.
        if let Some(ms) = p.handshake_ms {
            body = body.push(text(format!("handshake {ms}ms")).size(10).color(FG_MUTED));
        }

        // Unparsed lines. Amber rather than muted: this is a plugin talking past the
        // host, which is a real defect even though nothing failed.
        if p.unknown_lines > 0 {
            body = body.push(
                text(format!(
                    "{} line{} the host could not parse",
                    p.unknown_lines,
                    if p.unknown_lines == 1 { "" } else { "s" }
                ))
                .size(11)
                .color(AMBER),
            );
        }

        // The log tail. Bounded here as well as in `push_log`, because this is rendered
        // every frame the panel is visible and a 200-line block is not a panel, it is a
        // wall.
        const TAIL: usize = 6;
        if !p.log.is_empty() {
            let start = p.log.len().saturating_sub(TAIL);
            let mut log = column![].spacing(1);
            for line in &p.log[start..] {
                log = log.push(text(line).size(10).color(FG_MUTED));
            }
            body = body.push(log);
        }

        container(body)
            .padding(8)
            .width(Fill)
            .style(card_style)
            .into()
    }
}

/// One plugin that is not running, and the sentence saying why.
fn view_failed_plugin<'a>(name: &'a str, failure: &'a Failure) -> Element<'a, Message> {
    // Disabled is not a fault, so it is muted; everything else is. A disabled plugin in
    // red would train the user to ignore red.
    let colour = match failure {
        Failure::Disabled => FG_MUTED,
        _ => BAD,
    };
    let status = match failure {
        Failure::Disabled => "disabled",
        _ => "not running",
    };

    let mut head = row![
        text(name).size(12).color(FG),
        Space::new().width(Fill),
        text(status).size(11).color(colour),
    ]
    .spacing(8);
    // Only a DISABLED plugin gets an Enable button. Offering one on a plugin whose
    // program is missing or whose manifest is broken would be a button that changes a
    // key and fixes nothing — the failure is not the switch.
    if matches!(failure, Failure::Disabled) {
        head = head.push(
            button(text("Enable").size(11))
                .on_press(Message::SetPluginEnabled(name.to_string(), true))
                .padding([2, 8])
                .style(menu_item_style),
        );
    }

    container(column![head, text(failure.describe()).size(11).color(FG_MUTED)].spacing(4))
        .padding(8)
        .width(Fill)
        .style(card_style)
        .into()
}
