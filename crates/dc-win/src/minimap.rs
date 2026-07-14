//! The code-view **minimap** (VS Code style): a zoomed-out column on the right of the editor
//! showing the whole file's shape — each line a short bar (length ∝ code length) — with changed
//! lines glowing green (matching the PR highlight), comment ranges marked, and the current
//! viewport boxed. Clicking jumps the code view to that line.
//!
//! Pure rendering glue over data the app passes in; no logic lives here.

use std::collections::BTreeSet;

use ::iced::widget::canvas::{self as iced_canvas, Canvas, Frame, Geometry, Path, Stroke};
use ::iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

/// A minimap snapshot: the file's line lengths + which lines are changed/commented, plus the
/// visible viewport (fraction of the file scrolled to), so the box can be drawn. Owns its data
/// (rebuilt each frame) so the canvas has no borrow/lifetime plumbing.
pub struct Minimap {
    /// Length (chars) of each line, in order — drives each bar's width.
    line_lens: Vec<usize>,
    /// 1-based line numbers that differ from HEAD (drawn green).
    changed: BTreeSet<usize>,
    /// 1-based line numbers covered by an inline comment (drawn with an accent tick).
    commented: BTreeSet<usize>,
    /// The visible viewport as (top_fraction, height_fraction) of the whole file, for the box.
    /// `None` if unknown (no box drawn).
    viewport: Option<(f32, f32)>,
}

// Palette — match the app (Tokyo Night-ish). BG is semi-transparent so the code text runs
// behind the minimap (VS Code style — it floats over the right edge of the editor).
const BG: Color = Color::from_rgba(0.09, 0.10, 0.16, 0.55);
const BAR: Color = Color::from_rgba(0.62, 0.66, 0.80, 0.55);
const CHANGED: Color = Color::from_rgb(0.45, 0.78, 0.55);
const COMMENT: Color = Color::from_rgb(0.48, 0.65, 0.98);
const VIEWPORT: Color = Color::from_rgb(0.55, 0.60, 0.80);

impl Minimap {
    pub fn new(
        line_lens: Vec<usize>,
        changed: BTreeSet<usize>,
        commented: BTreeSet<usize>,
        viewport: Option<(f32, f32)>,
    ) -> Self {
        Self {
            line_lens,
            changed,
            commented,
            viewport,
        }
    }

    /// Build the minimap element (fills its narrow container).
    pub fn view(self) -> Element<'static, crate::app::Message> {
        Canvas::new(self)
            .width(Length::Fixed(72.0))
            .height(Length::Fill)
            .into()
    }

    /// The fraction (0..1) down the file that a y within `bounds` maps to.
    fn frac_at(bounds: Rectangle, y: f32) -> f32 {
        ((y - bounds.y) / bounds.height).clamp(0.0, 1.0)
    }
}

impl iced_canvas::Program<crate::app::Message> for Minimap {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        // Background.
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), BG);

        let n = self.line_lens.len().max(1);
        // Line height in the map: fit all lines, but never below ~1px so a long file still shows.
        let line_h = (bounds.height / n as f32).max(1.0);
        // The longest line drives the horizontal scale (clamped so one huge line doesn't squash
        // everything). Leave a small left margin.
        let max_len = self.line_lens.iter().copied().max().unwrap_or(1).max(1) as f32;
        let left = 4.0;
        let usable_w = (bounds.width - left - 4.0).max(4.0);

        for (i, &len) in self.line_lens.iter().enumerate() {
            let line = i + 1; // 1-based
            let y = i as f32 * line_h;
            // Leading-space indent: approximate by drawing the bar from a small offset. (We only
            // have length here; a full impl would carry indent — length alone still reads well.)
            let w = (len as f32 / max_len * usable_w).clamp(1.0, usable_w);
            let color = if self.changed.contains(&line) {
                CHANGED
            } else {
                BAR
            };
            frame.fill_rectangle(
                Point::new(left, y),
                Size::new(w, (line_h - 0.5).max(0.8)),
                color,
            );
            // Comment tick: a small accent square in the left margin.
            if self.commented.contains(&line) {
                frame.fill_rectangle(
                    Point::new(0.5, y),
                    Size::new(3.0, (line_h - 0.5).max(1.5)),
                    COMMENT,
                );
            }
        }

        // The viewport box.
        if let Some((top, height)) = self.viewport {
            let y = top.clamp(0.0, 1.0) * bounds.height;
            let h = (height.clamp(0.0, 1.0) * bounds.height).max(6.0);
            let rect = Path::rectangle(Point::new(0.5, y), Size::new(bounds.width - 1.0, h));
            frame.stroke(
                &rect,
                Stroke::default().with_color(VIEWPORT).with_width(1.0),
            );
            // A faint wash inside the viewport box.
            frame.fill_rectangle(
                Point::new(0.5, y),
                Size::new(bounds.width - 1.0, h),
                Color {
                    a: 0.06,
                    ..VIEWPORT
                },
            );
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        _state: &mut (),
        event: &iced_canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<iced_canvas::Action<crate::app::Message>> {
        if let iced_canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            if let Some(pos) = cursor.position_in(bounds) {
                // Map the click's vertical fraction to a 1-based line and emit a jump.
                let frac = Self::frac_at(
                    Rectangle {
                        x: 0.0,
                        y: 0.0,
                        ..bounds
                    },
                    pos.y,
                );
                let n = self.line_lens.len().max(1);
                let line = ((frac * n as f32) as usize + 1).min(n);
                // `.and_capture()` consumes the click so it does NOT fall through to the code's
                // per-line mouse_area beneath the floating minimap — otherwise a minimap click also
                // starts a line drag-select on the code underneath.
                return Some(
                    iced_canvas::Action::publish(crate::app::Message::MinimapJump(line))
                        .and_capture(),
                );
            }
        }
        None
    }
}
