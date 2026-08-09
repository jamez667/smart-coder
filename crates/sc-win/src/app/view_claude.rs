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
        const INPUT_H: f32 = 38.0;
        let bar = container(
            row![input, btn]
                .spacing(0)
                .align_y(iced::Alignment::Center)
                .height(Length::Fixed(INPUT_H)),
        )
        .width(Fill)
        .padding([0, 0]);

        column![stack, bar].width(Fill).height(Fill).into()
    }
}
