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
        let mut pushed: Vec<(
            String,
            String,
            sc_plugin_proto::Content,
            Option<sc_plugin_proto::Scroll>,
        )> = Vec::new();
        let mut cleared: Vec<(String, String)> = Vec::new();
        let mut stopped: Vec<(String, String)> = Vec::new();
        let mut requests: Vec<(String, PluginMessage)> = Vec::new();

        for p in plugins.running.iter_mut() {
            let plugin_id = p.manifest.as_ref().map(|m| m.id.clone());
            for ev in p.drain() {
                match ev {
                    PluginEvent::Message(m) => match *m {
                        PluginMessage::PanelContent {
                            panel,
                            content,
                            scroll,
                        } => {
                            if let Some(id) = plugin_id.clone() {
                                pushed.push((id, panel, content, scroll));
                            }
                        }
                        PluginMessage::ClearFields { panel } => {
                            if let Some(id) = plugin_id.clone() {
                                cleared.push((id, panel));
                            }
                        }
                        // A request. Answered below, outside this loop, because
                        // answering needs `&mut self` while this holds `&mut self.plugins`.
                        other => {
                            if let Some(id) = plugin_id.clone() {
                                requests.push((id, other));
                            }
                        }
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

        // Requests, answered in arrival order. EVERY request gets a reply, including a
        // failure — a dropped reply is a plugin waiting forever.
        for (plugin_id, msg) in requests {
            let Some(reply) = self.answer_plugin(&msg) else {
                continue;
            };
            if let Some(plugins) = self.plugins.as_mut() {
                if let Some(p) = plugins
                    .running
                    .iter_mut()
                    .find(|p| p.manifest.as_ref().is_some_and(|m| m.id == plugin_id))
                {
                    p.send(&reply);
                }
            }
        }

        // Field clears BEFORE content pushes, so a plugin that empties the composer and
        // pushes the sent message in the same breath does not have the clear undone by
        // its own push carrying the old value.
        for (plugin_id, panel_id) in cleared {
            let slug = format!("plugin:{plugin_id}:{panel_id}");
            if let Some(handle) = sc_craft_ui::plugin::registry::registry().from_slug(&slug) {
                self.plugin_fields.retain(|(p, _), _| *p != handle);
            }
        }

        for (plugin_id, panel_id, content, scroll) in pushed {
            let slug = format!("plugin:{plugin_id}:{panel_id}");
            if let Some(handle) = sc_craft_ui::plugin::registry::registry().from_slug(&slug) {
                // Flattened HERE, on arrival, not on paint: the limits and the nesting
                // walk are the expensive part, and doing them per frame would put the
                // plugin's content size on the render path.
                self.plugin_panels
                    .insert(handle, sc_craft_ui::plugin::flatten(&content));
                // A feed that streams asks to stay pinned to its tail. Recorded rather
                // than acted on here: the scroll is a Task, and this runs mid-drain.
                if scroll == Some(sc_plugin_proto::Scroll::Bottom) {
                    self.plugin_scroll_to_bottom.insert(handle);
                }
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

    /// Pin any panel whose plugin asked to be scrolled to the bottom.
    ///
    /// Drains the set: a request that stayed set would re-snap on every tick and fight
    /// the user the moment they scrolled up to read something. One push, one scroll.
    ///
    /// Batched, because two plugins can stream at once and each owns its own scrollable.
    pub(crate) fn plugin_autoscroll_task(&mut self) -> Task<Message> {
        if self.plugin_scroll_to_bottom.is_empty() {
            return Task::none();
        }
        let tasks: Vec<Task<Message>> = std::mem::take(&mut self.plugin_scroll_to_bottom)
            .into_iter()
            .map(|id| {
                iced::widget::operation::snap_to(
                    plugin_scroll_id(id),
                    iced::widget::scrollable::RelativeOffset { x: 0.0, y: 1.0 },
                )
            })
            .collect();
        Task::batch(tasks)
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
        container(scrollable(col).id(plugin_scroll_id(id)).height(Fill))
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
                severity,
                depth,
            } => {
                // The host decides what a severity LOOKS like — the plugin said what it
                // means. Same colours the Problems panel uses, so an error from a plugin
                // reads the same as one from the compiler.
                let colour = match severity {
                    Some(sc_plugin_proto::Severity::Error) => BAD,
                    Some(sc_plugin_proto::Severity::Warning) => AMBER,
                    Some(sc_plugin_proto::Severity::Info) | None => FG,
                };
                let mut line = row![text(label).size(12).color(colour)].spacing(8);
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

            // Through the app's OWN markdown renderer — the one the chat panel uses —
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
                // Enter sends (v2). Without this a composer needs a mouse trip to the
                // button for every message, which is why the host's own chat input has
                // bound Enter since it was written.
                if field.submit_on_enter {
                    input = input.on_submit(Message::PluginFormSubmit(id));
                }
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

/// The scrollable id for a plugin panel.
///
/// Stable per panel, so a scroll-to-bottom Task issued after a content push finds the
/// right one when several plugins are streaming at once. Mirrors `code_scroll_id`, which
/// exists for the same reason.
pub(crate) fn plugin_scroll_id(id: PluginPanelId) -> iced::advanced::widget::Id {
    // PER PANEL, not a singleton — `scroll_to` addresses a widget by id, so two plugin
    // panels sharing one would mean a push to either scrolling BOTH. The same bug
    // `code_scroll_id` documents, avoided the same way.
    //
    // `Id::new` takes `&'static str`; this is built at runtime, so go through
    // `From<String>`, which stores an owned `Cow`.
    iced::advanced::widget::Id::from(format!("plugin-panel:{}", id.0))
}

/// Indent a row by its nesting depth.
fn indent<'a>(depth: usize, el: Element<'a, Message>) -> Element<'a, Message> {
    if depth == 0 {
        return el;
    }
    row![Space::new().width(Length::Fixed(depth as f32 * INDENT)), el].into()
}
