//! The Settings modal — all that remains here of the middle column.
//!
//! This file was the agent's chat column: the thread, the composer, the gate controls,
//! the proposed-file cards, the swarm topology. All of it left for `sc-plugin-agent`
//! (spec 25), along with the Connections and Routing tabs, which configured model
//! endpoints the editor no longer has.
//!
//! What stayed is the modal frame and the General tab, because the editor still has
//! settings of its own to show.

use super::*;
use iced::widget::{column, row};

impl App {
    pub(crate) fn view_settings_body(&self) -> Element<'_, Message> {
        // No tab strip any more. Connections and Routing configured model endpoints and
        // left with the agent (spec 25), and a strip with one tab is a heading pretending
        // to be a chooser.
        let tabs = row![text("General").size(13).color(FG)].spacing(8);
        let body = self.view_general_tab();

        column![tabs, scrollable(body).height(Length::Fixed(400.0))]
            .spacing(12)
            .into()
    }

    /// The GENERAL tab.
    ///
    /// This used to lead with the Craft/Assistant switch — the setting that decided whether a
    /// model was ever contacted. There is no switch now: that choice is which executable you
    /// launched (spec 21), and this build is the one with the agent. Someone who wants the
    /// editor alone runs `smart-coder-crafter`, which has its own General tab with no model
    /// settings in it at all.
    pub(crate) fn view_general_tab(&self) -> Element<'_, Message> {
        let mut col = column![text("GENERAL").size(11).color(FG_MUTED)].spacing(10);

        // The Unity editor path. Only shown for a Unity project — a setting for a toolchain you
        // aren't using is noise, and the Hub convention finds it without help on most machines.

        if self.project_kind == sc_win::project::ProjectKind::Unity {
            let version = sc_win::project::unity_version(&self.workspace_root())
                .map(|v| format!("This project wants Unity {v}."))
                .unwrap_or_else(|| {
                    "ProjectSettings/ProjectVersion.txt doesn't name an editor version.".to_string()
                });
            col = col.push(text("UNITY").size(11).color(FG_MUTED));
            col = col.push(text(version).size(11).color(FG_MUTED));
            col = col.push(
                text_input(
                    "editor path (blank = find it via Unity Hub)",
                    &self.unity_path_input,
                )
                .on_input(Message::UnityPathChanged)
                .padding(6)
                .style(input_style),
            );
        }

        col.into()
    }
}
