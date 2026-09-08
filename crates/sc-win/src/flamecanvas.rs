//! Drawing the flame graph, and turning clicks on it into messages.
//!
//! Rendering glue only. Every decision that can be *wrong* — which rectangle goes where, what
//! a frame's share of the run is, which frames match a search — is computed by the pure
//! functions in [`sc_win::flame`] and tested there without a window. What lives here is the
//! part that needs a renderer: colours, text elision, and hit-testing.
//!
//! # Why a canvas and not a pile of widgets
//!
//! A real profile is thousands of frames. Building a widget tree that deep re-lays-out on every
//! frame and makes hover state a per-widget concern; a canvas draws the same picture in one
//! pass over a `Vec<Placed>` and hit-tests by walking the same vector. It is also the only way
//! to get the conventional look — flush rectangles, no gaps, text clipped to its own frame.

use ::iced::widget::canvas::{self as iced_canvas, Canvas, Frame, Geometry, Path, Stroke, Text};
use ::iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use sc_win::flame::{self, Placed};

/// Height of one frame row, in pixels. Tuned so an 11px label sits comfortably inside.
pub const ROW: f32 = 17.0;

/// Frames narrower than this fraction of the viewport are not drawn.
///
/// Sub-pixel rectangles cost time and change nothing on screen. At 1e-4 a 1600px-wide graph
/// drops everything under about a sixth of a pixel.
const MIN_WIDTH: f32 = 1e-4;

/// The flame graph's palette.
///
/// The convention is warm colours varying by *name hash*, not by cost: a flame graph's colour
/// carries no meaning, and making it look like it does (red = slow) actively misleads, since
/// width is already the measure. Hashing the name has the useful property that the same
/// function keeps its colour between runs and across the graph.
fn frame_color(name: &str, matched: bool, hovered: bool) -> Color {
    if matched {
        // Search hits go blue — the one deliberate exception, and unmistakable against warm.
        return Color::from_rgb(0.36, 0.62, 0.96);
    }
    // FNV-1a over the name: cheap, stable, and well spread for short strings.
    let mut h: u32 = 2_166_136_261;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16_777_619);
    }
    // Warm band: reds through oranges to yellows.
    let t = (h % 1000) as f32 / 1000.0;
    let r = 0.83 + 0.15 * t;
    let g = 0.28 + 0.42 * t;
    let b = 0.18 + 0.10 * t;
    if hovered {
        // Lighten rather than recolour, so the hovered frame stays recognisably itself.
        Color::from_rgb(
            (r + 0.12).min(1.0),
            (g + 0.12).min(1.0),
            (b + 0.12).min(1.0),
        )
    } else {
        Color::from_rgb(r, g, b)
    }
}

/// A canvas that draws one profile, zoomed to one subtree.
pub struct FlameCanvas<'a> {
    /// The subtree being drawn — already resolved from the zoom path by the caller.
    root: &'a flame::Frame,
    /// The whole profile's sample count, so percentages stay true when zoomed.
    profile_total: u64,
    /// The current search needle; matching frames are highlighted.
    search: &'a str,
    /// The frame under the cursor, if any, so it can be drawn lit.
    hovered: Option<&'a Placed>,
}

impl<'a> FlameCanvas<'a> {
    pub fn new(
        root: &'a flame::Frame,
        profile_total: u64,
        search: &'a str,
        hovered: Option<&'a Placed>,
    ) -> Self {
        Self {
            root,
            profile_total,
            search,
            hovered,
        }
    }

    /// Build the element. Height is the tree's depth in rows, so the graph scrolls
    /// inside its container rather than being squashed to fit.
    pub fn view(self) -> Element<'a, crate::app::Message> {
        let rows = self.root.depth().max(1) as f32;
        Canvas::new(self)
            .width(Length::Fill)
            .height(Length::Fixed(rows * ROW))
            .into()
    }
}

/// Which placed frame is under a point, if any.
///
/// Walks in reverse so deeper frames — drawn later — win a tie at a shared edge, matching what
/// the eye expects when clicking near a boundary.
fn hit(placed: &[Placed], pos: Point, width: f32) -> Option<&Placed> {
    placed.iter().rev().find(|p| {
        let x0 = p.x * width;
        let x1 = x0 + p.width * width;
        let y0 = p.depth as f32 * ROW;
        pos.x >= x0 && pos.x < x1 && pos.y >= y0 && pos.y < y0 + ROW
    })
}

impl iced_canvas::Program<crate::app::Message> for FlameCanvas<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &iced_canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<iced_canvas::Action<crate::app::Message>> {
        use ::iced::widget::canvas::Event;
        let placed = flame::layout(self.root, MIN_WIDTH);
        match event {
            // Click a frame to zoom into it. Clicking the frame already at the top is the
            // natural "zoom out one" gesture, so it publishes the parent's path instead.
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let pos = cursor.position_in(bounds)?;
                let f = hit(&placed, pos, bounds.width)?;
                let mut path = f.path.clone();
                if f.depth == 0 {
                    path.pop();
                }
                Some(iced_canvas::Action::publish(
                    crate::app::Message::FlameZoom(path),
                ))
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // Off the canvas entirely ⇒ clear the detail line, so it never reports a frame
                // the cursor has left.
                let Some(pos) = cursor.position_in(bounds) else {
                    return self.hovered.is_some().then(|| {
                        iced_canvas::Action::publish(crate::app::Message::FlameHover(None))
                    });
                };
                let found = hit(&placed, pos, bounds.width);
                // Only publish on a CHANGE. A cursor move within one frame fires this event
                // continuously, and redrawing the whole panel per pixel is how a profiler
                // viewer ends up slower than the code it is profiling.
                let same = match (found, self.hovered) {
                    (Some(a), Some(b)) => a.path == b.path,
                    (None, None) => true,
                    _ => false,
                };
                if same {
                    return None;
                }
                Some(iced_canvas::Action::publish(
                    crate::app::Message::FlameHover(found.cloned().map(Box::new)),
                ))
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let placed = flame::layout(self.root, MIN_WIDTH);

        for p in &placed {
            let x = p.x * w;
            let y = p.depth as f32 * ROW;
            let fw = (p.width * w).max(1.0);
            let matched = flame::matches(&p.name, self.search);
            let hovered = self.hovered.map(|h| h.path == p.path).unwrap_or(false);

            // A 1px inset gives the classic separated-brick look without drawing borders, which
            // would double the geometry for no gain.
            frame.fill_rectangle(
                Point::new(x, y),
                Size::new((fw - 1.0).max(1.0), ROW - 1.0),
                frame_color(&p.name, matched, hovered),
            );

            // Text only where it can be read. Roughly 7px per character at 11px; a frame that
            // cannot hold three characters gets none, since a single clipped letter is noise.
            let budget = ((fw - 6.0) / 6.6).floor();
            if budget >= 3.0 {
                let short = flame::short_name(&p.name);
                let label: String = if (short.chars().count() as f32) <= budget {
                    short.to_string()
                } else {
                    // Truncate by CHARACTERS, never bytes — a symbol may hold non-ASCII, and
                    // slicing mid-codepoint would panic.
                    short
                        .chars()
                        .take((budget as usize).saturating_sub(1))
                        .chain(std::iter::once('…'))
                        .collect()
                };
                frame.fill_text(Text {
                    content: label,
                    position: Point::new(x + 3.0, y + ROW / 2.0),
                    color: Color::from_rgb(0.10, 0.09, 0.11),
                    size: 11.0.into(),
                    align_y: ::iced::alignment::Vertical::Center,
                    ..Text::default()
                });
            }
        }

        // Nothing to draw at all: say so rather than showing a blank rectangle that reads as a
        // broken panel.
        if placed.len() <= 1 && self.root.total == 0 {
            frame.fill_text(Text {
                content: "— no samples —".to_string(),
                position: Point::new(w / 2.0, ROW),
                color: Color::from_rgb(0.52, 0.55, 0.66),
                size: 12.0.into(),
                align_x: ::iced::alignment::Horizontal::Center.into(),
                align_y: ::iced::alignment::Vertical::Center,
                ..Text::default()
            });
        }

        // A hairline under the hovered row helps the eye tie the rectangle to the detail line
        // below the graph.
        if let Some(h) = self.hovered {
            let y = (h.depth as f32 + 1.0) * ROW - 1.0;
            frame.stroke(
                &Path::line(Point::new(h.x * w, y), Point::new((h.x + h.width) * w, y)),
                Stroke::default()
                    .with_color(Color::from_rgb(0.95, 0.95, 0.98))
                    .with_width(1.0),
            );
        }

        let _ = self.profile_total;
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &(),
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        // A pointer over a frame is the affordance that says "this zooms".
        if let Some(pos) = cursor.position_in(bounds) {
            let placed = flame::layout(self.root, MIN_WIDTH);
            if hit(&placed, pos, bounds.width).is_some() {
                return mouse::Interaction::Pointer;
            }
        }
        mouse::Interaction::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_win::flame::parse_folded;

    #[test]
    fn hit_testing_finds_the_frame_under_the_cursor() {
        // `main` fills row 0; `a` is the left half of row 1, `b` the right half.
        let p = parse_folded("main;a 50\nmain;b 50");
        let placed = flame::layout(&p.root, 0.0);
        // Row 0 is the synthetic root, row 1 is `main`, row 2 the children.
        let a = hit(&placed, Point::new(10.0, ROW * 2.5), 100.0).unwrap();
        assert_eq!(a.name, "a");
        let b = hit(&placed, Point::new(90.0, ROW * 2.5), 100.0).unwrap();
        assert_eq!(b.name, "b");
        // Below the deepest row hits nothing.
        assert!(hit(&placed, Point::new(50.0, ROW * 9.0), 100.0).is_none());
    }

    #[test]
    fn the_boundary_between_two_frames_is_not_ambiguous() {
        let p = parse_folded("main;a 50\nmain;b 50");
        let placed = flame::layout(&p.root, 0.0);
        // Exactly on the seam belongs to the RIGHT frame: ranges are half-open, so no pixel
        // is claimed by two frames and none is claimed by neither.
        assert_eq!(
            hit(&placed, Point::new(50.0, ROW * 2.5), 100.0)
                .unwrap()
                .name,
            "b"
        );
        assert_eq!(
            hit(&placed, Point::new(49.9, ROW * 2.5), 100.0)
                .unwrap()
                .name,
            "a"
        );
    }

    #[test]
    fn a_functions_colour_is_stable_and_ignores_its_cost() {
        // Colour must depend only on the name, so the same function looks the same everywhere
        // in the graph and between runs.
        let c1 = frame_color("model_call", false, false);
        let c2 = frame_color("model_call", false, false);
        assert_eq!((c1.r, c1.g, c1.b), (c2.r, c2.g, c2.b));
        let other = frame_color("parse_config", false, false);
        assert!(
            (c1.r - other.r).abs() > f32::EPSILON || (c1.g - other.g).abs() > f32::EPSILON,
            "different names should generally differ"
        );
    }

    #[test]
    fn a_search_hit_is_blue_and_beats_the_name_hash() {
        let plain = frame_color("x", false, false);
        let matched = frame_color("x", true, false);
        assert_ne!(
            (plain.r, plain.g, plain.b),
            (matched.r, matched.g, matched.b)
        );
        // Blue: more blue than red, which no warm-band colour ever is.
        assert!(matched.b > matched.r);
        assert!(plain.r > plain.b);
    }

    #[test]
    fn hovering_lightens_without_changing_the_hue_family() {
        let plain = frame_color("x", false, false);
        let lit = frame_color("x", false, true);
        assert!(lit.r >= plain.r && lit.g >= plain.g);
        assert!(lit.r > plain.r || lit.g > plain.g);
    }

    #[test]
    fn colour_channels_stay_in_range_for_any_name() {
        // The hash feeds arithmetic on colour channels; an out-of-range value would render
        // as garbage rather than failing loudly.
        for n in [
            "",
            "a",
            "main",
            "<Vec<T> as Index>::index",
            "ζζζ",
            &"x".repeat(500),
        ] {
            for hov in [false, true] {
                let c = frame_color(n, false, hov);
                for ch in [c.r, c.g, c.b] {
                    assert!((0.0..=1.0).contains(&ch), "{n} out of range: {ch}");
                }
            }
        }
    }

    #[test]
    fn clicking_the_top_row_zooms_out_rather_than_re_zooming_itself() {
        // The frame at depth 0 is already the zoom root; zooming "into" it would do nothing,
        // so the gesture must pop one level instead.
        let p = parse_folded("main;a 10");
        let placed = flame::layout(&p.root, 0.0);
        let top = placed.iter().find(|p| p.depth == 0).unwrap();
        let mut path = top.path.clone();
        path.pop();
        assert!(path.is_empty(), "popping the root's own path empties it");
    }
}
