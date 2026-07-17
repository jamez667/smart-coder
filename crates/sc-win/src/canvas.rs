//! The swarm topology canvas: draws the advisor (left), the coders (center stack),
//! and the orchestrator (right) as boxes, with connector lines and arrowheads that
//! glow when a message flows between them (orchestrator→coder on dispatch/integration,
//! advisor→coder on consult). Pure rendering glue over [`sc_win::Topology`]; the
//! fold/decay logic and its tests live in `sc_win::topology`.

use ::iced::widget::canvas::{self as iced_canvas, Canvas, Frame, Geometry, Path, Stroke, Text};
use ::iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme, Vector};

use sc_win::topology::{CoderState, Peer};
use sc_win::Topology;

/// A canvas program that draws a snapshot of the topology at elapsed time `now`.
/// Clicking a coder box publishes [`crate::app::Message::SelectCoder`]; `selected` is
/// the currently-selected coder id (drawn highlighted).
pub struct TopologyCanvas<'a> {
    topo: &'a Topology,
    now: f32,
    selected: Option<&'a str>,
}

impl<'a> TopologyCanvas<'a> {
    pub fn new(topo: &'a Topology, now: f32, selected: Option<&'a str>) -> Self {
        Self {
            topo,
            now,
            selected,
        }
    }

    /// Build the canvas element (fills its container).
    pub fn view(self) -> Element<'a, crate::app::Message> {
        Canvas::new(self)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

// Palette (Tokyo Night-ish, to match the app theme).
const BG_NODE: Color = Color::from_rgb(0.15, 0.16, 0.24);
const TEXT: Color = Color::from_rgb(0.86, 0.88, 0.95);
const ORCH: Color = Color::from_rgb(0.48, 0.65, 0.98); // blue
const ADVISOR: Color = Color::from_rgb(0.74, 0.58, 0.98); // purple
const GOOD: Color = Color::from_rgb(0.45, 0.78, 0.55); // green
const BAD: Color = Color::from_rgb(0.93, 0.42, 0.42); // red
const DIM: Color = Color::from_rgb(0.32, 0.34, 0.46);

const NODE_W: f32 = 150.0;
const NODE_H: f32 = 54.0;

/// The center point of coder box `i` of `n`, in a canvas of width `w`, height `h`.
/// Shared by `draw` and the click hit-test so they always agree.
fn coder_center(i: usize, n: usize, w: f32, h: f32) -> Point {
    let top = 80.0;
    let usable = (h - top - 30.0).max(NODE_H);
    let gap = if n > 1 {
        (usable - NODE_H) / (n as f32 - 1.0)
    } else {
        0.0
    };
    Point::new(w / 2.0, top + NODE_H / 2.0 + gap * i as f32)
}

/// Whether `p` is inside the box centered at `c`.
fn in_box(p: Point, c: Point) -> bool {
    (p.x - c.x).abs() <= NODE_W / 2.0 && (p.y - c.y).abs() <= NODE_H / 2.0
}

fn coder_color(state: CoderState) -> Color {
    match state {
        CoderState::Working => ORCH,
        CoderState::Done => TEXT,
        CoderState::Retrying => BAD,
        CoderState::Integrated => GOOD,
        CoderState::Reverted => BAD,
    }
}

fn state_label(state: CoderState) -> &'static str {
    match state {
        CoderState::Working => "working",
        CoderState::Done => "proposed",
        CoderState::Retrying => "retrying",
        CoderState::Integrated => "integrated",
        CoderState::Reverted => "reverted",
    }
}

/// Blend `c` toward white by `t` (for a glow highlight).
fn brighten(c: Color, t: f32) -> Color {
    Color::from_rgb(
        c.r + (1.0 - c.r) * t,
        c.g + (1.0 - c.g) * t,
        c.b + (1.0 - c.b) * t,
    )
}

impl iced_canvas::Program<crate::app::Message> for TopologyCanvas<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &iced_canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<iced_canvas::Action<crate::app::Message>> {
        use ::iced::widget::canvas::Event;
        // On a left click inside a coder box, select it (publish the message).
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            let pos = cursor.position_in(bounds)?;
            let coders = self.topo.coders();
            let n = coders.len().max(1);
            for (i, coder) in coders.iter().enumerate() {
                if in_box(pos, coder_center(i, n, bounds.width, bounds.height)) {
                    return Some(iced_canvas::Action::publish(
                        crate::app::Message::SelectCoder(coder.subtask.clone()),
                    ));
                }
            }
            // Clicked empty space → clear the selection.
            return Some(iced_canvas::Action::publish(
                crate::app::Message::ClearSelection,
            ));
        }
        None
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
        let w = frame.width();
        let h = frame.height();

        if self.topo.is_empty() {
            frame.fill_text(Text {
                content: "— start a swarm to see the coders, advisor and orchestrator —"
                    .to_string(),
                position: Point::new(w / 2.0 - 220.0, h / 2.0),
                color: DIM,
                size: 14.0.into(),
                ..Text::default()
            });
            return vec![frame.into_geometry()];
        }

        let coders = self.topo.coders();
        let n = coders.len().max(1);

        // Column x-centers: advisor left, coders center, orchestrator right.
        let advisor_cx = NODE_W / 2.0 + 24.0;
        let orch_cx = w - NODE_W / 2.0 - 24.0;
        let coder_center = |i: usize| coder_center(i, n, w, h);

        let advisor_c = Point::new(advisor_cx, h / 2.0);
        let orch_c = Point::new(orch_cx, 70.0);

        // --- Edges first (so nodes draw on top of the lines) ---
        // Static connectors (dim), then glowing flows on top.
        for (i, _coder) in coders.iter().enumerate() {
            let cc = coder_center(i);
            draw_connector(&mut frame, orch_c, cc, DIM, 1.0, 0.18);
            if self.topo.advisor_used {
                draw_connector(&mut frame, advisor_c, cc, DIM, 1.0, 0.18);
            }
        }
        // Glowing message flows.
        for (flow, intensity) in self.topo.active_flows(self.now) {
            if let Some(i) = coders.iter().position(|c| c.subtask == flow.coder) {
                let cc = coder_center(i);
                let (from, base) = match flow.peer {
                    Peer::Orchestrator => (orch_c, ORCH),
                    Peer::Advisor => (advisor_c, ADVISOR),
                };
                let col = brighten(base, intensity * 0.5);
                draw_connector(
                    &mut frame,
                    from,
                    cc,
                    col,
                    1.0 + 2.5 * intensity,
                    0.35 + 0.65 * intensity,
                );
                draw_arrowhead(&mut frame, from, cc, col, intensity);
            }
        }

        // --- Nodes ---
        draw_node(
            &mut frame,
            orch_c,
            "ORCHESTRATOR",
            "decomposes · integrates",
            ORCH,
        );
        if self.topo.advisor_used {
            draw_node(
                &mut frame,
                advisor_c,
                "ADVISOR",
                "senior · on stall",
                ADVISOR,
            );
        } else {
            draw_node(&mut frame, advisor_c, "ADVISOR", "(idle)", DIM);
        }
        for (i, coder) in coders.iter().enumerate() {
            let cc = coder_center(i);
            let sub = format!("{} · {}", coder.subtask, state_label(coder.state));
            draw_node(
                &mut frame,
                cc,
                &short(&coder.goal, coder),
                &sub,
                coder_color(coder.state),
            );
            // A bright ring around the selected coder.
            if self.selected == Some(coder.subtask.as_str()) {
                let tl = Point::new(cc.x - NODE_W / 2.0 - 3.0, cc.y - NODE_H / 2.0 - 3.0);
                frame.stroke(
                    &Path::rectangle(tl, Size::new(NODE_W + 6.0, NODE_H + 6.0)),
                    Stroke::default().with_color(TEXT).with_width(2.0),
                );
            }
        }

        vec![frame.into_geometry()]
    }
}

/// A short top-line label for a coder box: its goal, trimmed, or its id if no goal.
fn short(goal: &str, coder: &sc_win::topology::Coder) -> String {
    let g = goal.trim();
    if g.is_empty() {
        format!("coder {}", coder.subtask)
    } else if g.chars().count() > 22 {
        // Truncate by chars (not bytes) so a multibyte boundary never panics.
        let head: String = g.chars().take(21).collect();
        format!("{head}…")
    } else {
        g.to_string()
    }
}

/// Draw a node box centered at `c` with a title and subtitle, accent `color`.
fn draw_node(frame: &mut Frame, c: Point, title: &str, subtitle: &str, color: Color) {
    let top_left = Point::new(c.x - NODE_W / 2.0, c.y - NODE_H / 2.0);
    let size = Size::new(NODE_W, NODE_H);
    frame.fill_rectangle(top_left, size, BG_NODE);
    frame.stroke(
        &Path::rectangle(top_left, size),
        Stroke::default().with_color(color).with_width(1.5),
    );
    // A small accent bar on the left edge.
    frame.fill_rectangle(top_left, Size::new(4.0, NODE_H), color);

    frame.fill_text(Text {
        content: title.to_string(),
        position: Point::new(top_left.x + 12.0, c.y - 14.0),
        color: TEXT,
        size: 13.0.into(),
        ..Text::default()
    });
    frame.fill_text(Text {
        content: subtitle.to_string(),
        position: Point::new(top_left.x + 12.0, c.y + 2.0),
        color,
        size: 11.0.into(),
        ..Text::default()
    });
}

/// Draw a connector line between two node centers, clipped to their edges, with the
/// given color, width, and alpha.
fn draw_connector(frame: &mut Frame, from: Point, to: Point, color: Color, width: f32, alpha: f32) {
    let (a, b) = edge_points(from, to);
    let mut c = color;
    c.a = alpha;
    frame.stroke(
        &Path::line(a, b),
        Stroke::default().with_color(c).with_width(width),
    );
}

/// Draw an arrowhead near the `to` end of the `from→to` connector.
fn draw_arrowhead(frame: &mut Frame, from: Point, to: Point, color: Color, intensity: f32) {
    let (_a, b) = edge_points(from, to);
    let dir = normalize(Vector::new(b.x - from.x, b.y - from.y));
    let tip = b;
    let back = Point::new(tip.x - dir.x * 12.0, tip.y - dir.y * 12.0);
    // Perpendicular.
    let perp = Vector::new(-dir.y, dir.x);
    let left = Point::new(back.x + perp.x * 6.0, back.y + perp.y * 6.0);
    let right = Point::new(back.x - perp.x * 6.0, back.y - perp.y * 6.0);
    let head = Path::new(|p| {
        p.move_to(tip);
        p.line_to(left);
        p.line_to(right);
        p.close();
    });
    let mut c = color;
    c.a = 0.4 + 0.6 * intensity;
    frame.fill(&head, c);
}

/// The points where the center-to-center line crosses each node's bounding box, so
/// connectors touch box edges rather than centers. A simple inset along the direction.
fn edge_points(from: Point, to: Point) -> (Point, Point) {
    let dir = normalize(Vector::new(to.x - from.x, to.y - from.y));
    let inset = NODE_W / 2.0 + 2.0;
    let a = Point::new(from.x + dir.x * inset, from.y + dir.y * inset);
    let b = Point::new(to.x - dir.x * inset, to.y - dir.y * inset);
    (a, b)
}

fn normalize(v: Vector) -> Vector {
    let len = (v.x * v.x + v.y * v.y).sqrt();
    if len < 1e-3 {
        Vector::new(0.0, 0.0)
    } else {
        Vector::new(v.x / len, v.y / len)
    }
}
