//! The heads-up display: speed, hull, and the docking prompt.

/// Draw the HUD over the world. Text is laid out from the top-left corner and
/// the bars are drawn last so nothing overlaps them.
pub fn draw_hud(batch: &mut Batch, speed: f32, hull: f32) {
    draw_speed_readout(batch, speed);
    draw_hull_bar(batch, hull);
}

/// The numeric speed readout, in metres per second.
fn draw_speed_readout(batch: &mut Batch, speed: f32) {
    batch.text(format!("{speed:.0} m/s"));
}

/// A horizontal bar showing remaining hull integrity, red below a quarter.
fn draw_hull_bar(batch: &mut Batch, hull: f32) {
    let colour = if hull < 0.25 { RED } else { GREEN };
    batch.rect_filled(hull, colour);
}
