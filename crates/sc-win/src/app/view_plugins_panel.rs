//! The Plugins panel (spec 25): what loaded, what did not, and why.
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

use super::*;
use iced::widget::{column, row};

use sc_craft_ui::plugin::Failure;

impl App {
    /// The PLUGINS tab of the bottom strip.
    pub(crate) fn view_plugins_tab(&self) -> Element<'_, Message> {
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
            .height(Fill)
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
            return container(scrollable(col).height(Fill))
                .width(Fill)
                .height(Fill)
                .into();
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

        container(scrollable(col).height(Fill))
            .width(Fill)
            .height(Fill)
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

    container(
        column![
            row![
                text(name).size(12).color(FG),
                Space::new().width(Fill),
                text(status).size(11).color(colour),
            ]
            .spacing(8),
            text(failure.describe()).size(11).color(FG_MUTED),
        ]
        .spacing(4),
    )
    .padding(8)
    .width(Fill)
    .style(card_style)
    .into()
}
