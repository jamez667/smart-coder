//! The compliance-report dialog: pick a model, run the audit, open the result.
//!
//! Kept in its own file rather than added to `view_panels.rs`, which is already
//! near this crate's ~800-line ceiling.
//!
//! The dialog exists because of one thing: the model choice needs a caveat next
//! to it. Firing the audit straight off the menu item would have been less code,
//! but there would be nowhere honest to say what a model does and does not do to
//! a compliance document.

use super::*;
use iced::widget::{column, row};

use sc_win::comply::ComplyModel;

impl App {
    pub(crate) fn view_comply_modal(&self) -> Element<'_, Message> {
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
            .on_press(Message::CloseComplyDialog);

        let header = row![
            text("Compliance report").size(16).color(FG),
            Space::new().width(Fill),
            button(text("✕").size(14))
                .on_press(Message::CloseComplyDialog)
                .padding([2, 8])
                .style(menu_item_style),
        ]
        .align_y(iced::Alignment::Center);

        let card = container(column![header, self.view_comply_body()].spacing(12))
            .width(Length::Fixed(520.0))
            .max_width(560.0)
            .padding(18)
            .style(dropdown_style);

        iced::widget::stack![backdrop, iced::widget::center(iced::widget::opaque(card))]
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn view_comply_body(&self) -> Element<'_, Message> {
        let mut col = column![].spacing(10);

        col = col.push(
            text(format!(
                "Audits this project against all {} shipped frameworks and writes a \
                 redacted HTML report to docs/compliance.",
                sc_comply::registry::SHIPPED.len()
            ))
            .size(12)
            .color(FG_MUTED),
        );

        // The picker.
        col = col.push(text("Summary written by").size(12).color(FG));
        let seg = |m: ComplyModel| {
            let active = self.comply_model == m;
            let mut b = button(text(m.label().to_string()).size(12).color(if active {
                FG
            } else {
                FG_MUTED
            }))
            .padding([4, 14])
            .style(if active {
                primary_button
            } else {
                stage_toggle_button
            });
            // Disabled mid-run: changing the model would not affect the audit
            // already in flight, so offering it would be a lie.
            if !self.comply_running {
                b = b.on_press(Message::ComplyModelChanged(m));
            }
            b
        };
        col = col.push(
            row![
                seg(ComplyModel::None),
                seg(ComplyModel::Local),
                seg(ComplyModel::Gemini)
            ]
            .spacing(6),
        );

        // What the choice actually means. This is the reason the dialog exists.
        col = col.push(match self.comply_model.caveat() {
            Some(c) => text(c).size(11).color(AMBER),
            None => text(
                "Deterministic only — no executive summary or auditor guidance. \
                 Every control result is still produced.",
            )
            .size(11)
            .color(FG_MUTED),
        });

        col = col.push(
            text(
                "Control results never use a model. A model only writes the executive \
                 summary and the guidance for controls a code scan cannot settle.",
            )
            .size(11)
            .color(FG_MUTED),
        );

        // Run / running.
        col = col.push(Space::new().height(Length::Fixed(4.0)));
        let mut run = button(
            text(if self.comply_running {
                "Auditing…"
            } else {
                "🛡  Generate report"
            })
            .size(14)
            .color(FG),
        )
        .padding([6, 16]);
        if self.comply_running {
            run = run.style(stage_toggle_button);
        } else {
            run = run.on_press(Message::RunComply).style(primary_button);
        }
        col = col.push(run);

        if self.comply_running {
            col = col.push(
                text(if self.comply_model == ComplyModel::None {
                    "Scanning the workspace…"
                } else {
                    "Scanning the workspace, then writing the summary. \
                     The model calls take a while."
                })
                .size(11)
                .color(FG_MUTED),
            );
        }

        if let Some(result) = &self.comply_result {
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            col = col.push(self.view_comply_result(result));
        }

        col.into()
    }

    /// The outcome: totals and an Open button, or the reason it failed.
    fn view_comply_result<'a>(
        &'a self,
        result: &'a Result<sc_win::comply::ComplyReport, String>,
    ) -> Element<'a, Message> {
        match result {
            Err(e) => column![
                text("Could not generate the report").size(13).color(BAD),
                text(e.clone()).size(11).color(FG_MUTED),
            ]
            .spacing(4)
            .into(),
            Ok(r) => {
                let mut col = column![].spacing(4);
                col = col.push(
                    text(format!(
                        "{} controls across {} frameworks",
                        r.controls, r.frameworks
                    ))
                    .size(13)
                    .color(FG),
                );
                // Counts, never a single headline percentage — the same rule the
                // reports themselves follow. "78% compliant" is the misreading
                // this whole feature exists to avoid.
                col = col.push(
                    row![
                        text(format!("{} verified", r.passed)).size(12).color(GOOD),
                        text(format!("{} gaps", r.gaps)).size(12).color(BAD),
                        text(format!("{} need manual evidence", r.unknown))
                            .size(12)
                            .color(AMBER),
                    ]
                    .spacing(12),
                );
                if !r.narrated && self.comply_model != ComplyModel::None {
                    // The user asked for a summary and did not get one. Say so
                    // rather than letting them assume the page has one.
                    col = col.push(
                        text(
                            "The model did not return a usable summary — the report is \
                              published without one.",
                        )
                        .size(11)
                        .color(AMBER),
                    );
                }
                col = col.push(
                    button(text("Open report").size(13).color(FG))
                        .on_press(Message::OpenComplyReport)
                        .padding([4, 12])
                        .style(primary_button),
                );
                col.into()
            }
        }
    }
}
