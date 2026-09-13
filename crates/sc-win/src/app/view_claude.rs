//! The markdown renderer — all that remains here of the Claude Code panel.
//!
//! The panel is a plugin now (spec 25): its feed, its composer and its options menu live
//! in `sc-plugin-claude` and arrive over the wire. What could not go with them is the
//! *rendering* — a plugin sends `Content::Text { markdown }` and the host draws it — so
//! this is now shared by every plugin with prose to show rather than belonging to one
//! panel.
//!
//! That sharing is not a consolation prize; it is why `text` was a v1 content kind at
//! all. The capability already existed and was already proven, so a plugin's prose looks
//! exactly like the agent's for free.

use super::*;
use iced::widget::{column, row};

/// Render Markdown prose as a styled column: headings, bullets, code and gaps.
///
/// The panel used to push a whole answer through as one flat line of size-12 text,
/// which is why a page of it read as an undifferentiated wall next to any other
/// chat client. Only the structure a coding model actually emits is handled — see
/// `sc_win::markdown` for why the parser stops there.
pub(crate) fn markdown_body(src: &str) -> Element<'static, Message> {
    use sc_win::markdown::{parse, Block};

    let mut col = column![].spacing(2).width(Fill);
    for block in parse(src) {
        let rendered: Element<'static, Message> = match block {
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
            Block::Bullet { spans } => container(
                row![text("•").size(12).color(FG_MUTED), inline(spans, 12.0, FG)]
                    .spacing(6)
                    .align_y(iced::Alignment::Start),
            )
            .padding(iced::Padding::default().top(1).bottom(1))
            .into(),
            // A code LINE. Zero vertical padding so consecutive lines butt together
            // into ONE box -- with padding each line became a separately-inset strip
            // with a gap above and below, which reads as a stack of fragments rather
            // than as a block of code.
            Block::Code(line) => {
                container(text(line).size(11).font(iced::Font::MONOSPACE).color(GOOD))
                    .width(Fill)
                    .padding([0, 8])
                    .style(|_: &iced::Theme| container::Style {
                        background: Some(EDITOR_BG.into()),
                        ..Default::default()
                    })
                    .into()
            }
            // A table row as a real row of columns. Each cell takes an equal share of
            // the width, so successive rows line up without measuring text. The
            // header is brighter and tinted; body rows sit on the feed's own
            // background so a long table does not stripe.
            Block::TableRow { head, cells } => {
                let mut r = row![].spacing(10).width(Fill);
                for cell in cells {
                    r = r.push(
                        container(inline(cell, 12.0, if head { Color::WHITE } else { FG }))
                            .width(Length::FillPortion(1)),
                    );
                }
                container(r)
                    .width(Fill)
                    .padding([2, 6])
                    .style(move |_: &iced::Theme| container::Style {
                        background: head.then(|| EDITOR_BG.into()),
                        ..Default::default()
                    })
                    .into()
            }
            Block::Para { spans } => container(inline(spans, 12.0, FG))
                .padding(iced::Padding::default().top(2).bottom(2))
                .into(),
        };
        col = col.push(rendered);
    }
    col.into()
}

/// One line's spans as a wrapping row: plain, **bold**, and `code`.
///
/// A `row` rather than a single `text`, because iced styles a whole `text` widget
/// at once — mixed emphasis inside one line needs one widget per run.
fn inline(spans: Vec<sc_win::markdown::Span>, size: f32, fg: Color) -> Element<'static, Message> {
    use sc_win::markdown::Span;

    // ONE WIDGET PER WORD, not per span.
    //
    // A `wrap()`ping row can only break BETWEEN its children, so a span holding a
    // whole sentence is an unbreakable block: the line broke at span boundaries
    // instead of at words, and a paragraph with inline code came out as orphaned
    // fragments — `, and both my clamp…` alone on a line after a code span pushed
    // it over. Splitting prose into words gives the layout somewhere to break.
    //
    // Code and bold spans stay whole: breaking mid-identifier is worse than a
    // slightly ragged edge, and they are short.
    let mut r = row![].spacing(0).align_y(iced::Alignment::Center);
    for sp in spans {
        match sp {
            Span::Plain(t) => {
                // `split_inclusive(' ')` keeps the trailing space ON the word, so the
                // gaps survive with `spacing(0)` — a plain `split` would need spacing,
                // which would then also appear either side of code spans that have no
                // space in the source.
                for word in t.split_inclusive(' ') {
                    r = r.push(text(word.to_string()).size(size).color(fg));
                }
            }
            // Brighter rather than a bold face: the UI ships one weight, so weight is
            // not available and contrast is what reads as emphasis.
            Span::Bold(t) => r = r.push(text(t).size(size).color(Color::WHITE)),
            // Dimmer and monospace rather than the old green, which read as syntax
            // highlighting in a chat rather than as an inline literal.
            Span::Code(t) => {
                r = r.push(
                    text(t)
                        .size(size - 1.0)
                        .font(iced::Font::MONOSPACE)
                        .color(AMBER),
                )
            }
        }
    }
    // `wrap()` so a long line breaks at the panel edge instead of running past it.
    r.wrap().into()
}
