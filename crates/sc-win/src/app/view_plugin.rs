//! Rendering a plugin's panel (spec 25).
//!
//! The plugin pushed a [`Content`]; `sc_craft_ui::plugin::view::flatten` turned it into
//! flat rows with the limits already applied; this builds the widgets.
//!
//! **The host owns the styling.** A plugin says "a row, with this detail, running this
//! command"; it does not say what a row looks like. That is what makes a plugin panel
//! look like the rest of the app rather than like a web page someone embedded — and it
//! is why the content model has four kinds and no colours (spec 25).
//!
//! Nothing here reaches the plugin. Rendering is from the cached last-pushed content, so
//! a slow or dead plugin costs nothing at paint time; a synchronous round trip on the
//! render path is the mistake the file-tree cache and the sync-interval change both
//! exist to avoid.

use super::*;
use iced::widget::{column, row};

use sc_craft_ui::plugin::view::Row;
use sc_craft_ui::plugin::PluginPanelId;

/// Indent per nesting level, in pixels.
///
/// Small on purpose: nesting in a plugin panel is composition, not hierarchy, and a deep
/// indent would waste the narrow width a side panel usually has.
const INDENT: f32 = 12.0;

impl App {
    /// The rows a plugin panel should draw, or `None` when the plugin has pushed nothing.
    ///
    /// `None` and `Some(empty)` are different states and the panel says so differently:
    /// a plugin that has not pushed yet is starting up, while one that pushed an empty
    /// list is telling you it found nothing.
    pub(crate) fn plugin_content(&self, id: PluginPanelId) -> Option<&Vec<Row>> {
        self.plugin_panels.get(&id)
    }

    /// Drain every running plugin's messages and apply them.
    ///
    /// Called on the UI tick. Batched per plugin (see `Plugin::drain`) because a plugin
    /// streaming a feed pushes faster than the tick fires, and taking one message per
    /// tick would fall steadily further behind.
    pub(crate) fn pump_plugins(&mut self) {
        use sc_craft_ui::plugin::PluginEvent;
        use sc_plugin_proto::PluginMessage;

        let Some(plugins) = self.plugins.as_mut() else {
            return;
        };
        // Collected first, then applied, because applying needs `&mut self` while
        // draining holds `&mut self.plugins`.
        let mut pushed: Vec<(String, String, sc_plugin_proto::Content)> = Vec::new();
        let mut stopped: Vec<(String, String)> = Vec::new();

        for p in plugins.running.iter_mut() {
            let plugin_id = p.manifest.as_ref().map(|m| m.id.clone());
            for ev in p.drain() {
                match ev {
                    PluginEvent::Message(m) => match *m {
                        PluginMessage::PanelContent { panel, content } => {
                            if let Some(id) = plugin_id.clone() {
                                pushed.push((id, panel, content));
                            }
                        }
                        // Everything else in v1 is a request needing a reply, which is
                        // the next slice of work. Logged rather than dropped silently so
                        // a plugin using an unimplemented call can see that it arrived.
                        other => p.push_log(format!("unhandled: {other:?}")),
                    },
                    PluginEvent::Ready(_) => {}
                    PluginEvent::Stopped(r) => {
                        let why = match r {
                            Ok(()) => "exited".to_string(),
                            Err(e) => e,
                        };
                        stopped.push((p.dir_name.clone(), why));
                    }
                }
            }
        }

        for (plugin_id, panel_id, content) in pushed {
            let slug = format!("plugin:{plugin_id}:{panel_id}");
            if let Some(handle) = sc_craft_ui::plugin::registry::registry().from_slug(&slug) {
                // Flattened HERE, on arrival, not on paint: the limits and the nesting
                // walk are the expensive part, and doing them per frame would put the
                // plugin's content size on the render path.
                self.plugin_panels
                    .insert(handle, sc_craft_ui::plugin::flatten(&content));
            }
        }
        if !stopped.is_empty() {
            if let Some(plugins) = self.plugins.as_mut() {
                // A stopped plugin leaves its panels showing their last content rather
                // than blanking them. The content was true when it was pushed, and a
                // panel that empties itself on a crash destroys the evidence of what the
                // plugin was doing when it died.
                plugins
                    .running
                    .retain(|p| !stopped.iter().any(|(name, _)| *name == p.dir_name));
                for (name, why) in stopped {
                    plugins
                        .failed
                        .push((name, sc_craft_ui::plugin::Failure::Stopped(why)));
                }
            }
        }
    }

    /// Send a message to whichever plugin owns `panel`.
    ///
    /// The closure receives the panel's own id (the plugin-local one, not the interned
    /// handle), because that is what the plugin declared and what it expects back.
    ///
    /// Silently does nothing when the panel or its plugin is gone. That is the right
    /// behaviour rather than an oversight: a click on a panel whose plugin has just
    /// crashed should do nothing, not raise an error the user cannot act on — the
    /// Plugins panel is where a dead plugin is reported.
    pub(crate) fn send_to_plugin_of(
        &mut self,
        panel: PluginPanelId,
        build: impl Fn(String) -> sc_plugin_proto::HostMessage,
    ) {
        let Some(registered) = sc_craft_ui::plugin::registry::registry().get(panel) else {
            return;
        };
        let (plugin_id, panel_id) = (registered.plugin_id.clone(), registered.panel_id.clone());
        let Some(plugins) = self.plugins.as_mut() else {
            return;
        };
        let Some(p) = plugins
            .running
            .iter_mut()
            .find(|p| p.manifest.as_ref().is_some_and(|m| m.id == plugin_id))
        else {
            return;
        };
        p.send(&build(panel_id));
    }

    /// The in-progress value of a plugin form field, if the user has typed into it.
    ///
    /// `None` ⇒ nothing typed yet, so the field shows what the plugin sent. That
    /// distinction is what lets a plugin push new content without discarding a
    /// half-typed value the user has not submitted.
    fn plugin_field_value(&self, id: PluginPanelId, field_id: &str) -> Option<&String> {
        self.plugin_fields.get(&(id, field_id.to_string()))
    }

    /// A plugin's panel.
    pub(crate) fn view_plugin_panel(&self, id: PluginPanelId) -> Element<'_, Message> {
        let Some(rows) = self.plugin_content(id) else {
            // Distinct from an empty list: this plugin has not spoken yet.
            return container(text("Waiting for the plugin…").size(11).color(FG_MUTED))
                .padding(8)
                .width(Fill)
                .height(Fill)
                .into();
        };

        if rows.is_empty() {
            return container(text("Nothing to show.").size(11).color(FG_MUTED))
                .padding(8)
                .width(Fill)
                .height(Fill)
                .into();
        }

        let mut col = column![].spacing(2).padding(6);
        for r in rows {
            col = col.push(self.view_plugin_row(id, r));
        }
        container(scrollable(col).height(Fill))
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// One row.
    fn view_plugin_row<'a>(&'a self, id: PluginPanelId, r: &'a Row) -> Element<'a, Message> {
        match r {
            Row::Item {
                text: label,
                detail,
                command,
                args,
                depth,
            } => {
                let mut line = row![text(label).size(12).color(FG)].spacing(8);
                if let Some(d) = detail {
                    line = line.push(Space::new().width(Fill));
                    line = line.push(text(d).size(11).color(FG_MUTED));
                }
                let body = container(line).padding([2, 4]).width(Fill);
                match command {
                    // A clickable row is a button; a plain one is not. Rendering every
                    // row as a button would make the whole panel look interactive when
                    // most of it is not.
                    Some(cmd) => indent(
                        *depth,
                        button(body)
                            .on_press(Message::PluginCommand(id, cmd.clone(), args.clone()))
                            .style(menu_item_style)
                            .width(Fill)
                            .into(),
                    ),
                    None => indent(*depth, body.into()),
                }
            }

            // Through the app's OWN markdown renderer — the one the Claude panel uses —
            // so a plugin's prose looks exactly like the agent's. This reuse is the
            // reason `text` is a v1 content kind rather than a deferred one.
            Row::Markdown { source, depth } => {
                indent(*depth, super::view_claude::markdown_body(source))
            }

            Row::Field { field, depth } => {
                let mut input = text_input(
                    field.placeholder.as_deref().unwrap_or(""),
                    self.plugin_field_value(id, &field.id)
                        .unwrap_or(&field.value),
                )
                .on_input(move |v| Message::PluginFieldChanged(id, field.id.clone(), v))
                .padding(6)
                .style(input_style);
                if field.secret {
                    input = input.secure(true);
                }
                indent(
                    *depth,
                    column![text(&field.label).size(11).color(FG_MUTED), input]
                        .spacing(4)
                        .into(),
                )
            }

            Row::Submit { label, depth } => indent(
                *depth,
                button(text(label).size(12))
                    .on_press(Message::PluginFormSubmit(id))
                    .style(primary_button)
                    .into(),
            ),

            // The truncation warning and the unsupported-kind note. Muted, because they
            // are the host talking, not the plugin.
            Row::Note { text: t } => container(text(t).size(11).color(FG_MUTED))
                .padding([4, 4])
                .into(),
        }
    }
}

/// Indent a row by its nesting depth.
fn indent<'a>(depth: usize, el: Element<'a, Message>) -> Element<'a, Message> {
    if depth == 0 {
        return el;
    }
    row![Space::new().width(Length::Fixed(depth as f32 * INDENT)), el].into()
}
