//! Screen-space bloom: bright pixels bleed into their neighbours.

use crate::math::Vec2;

/// How far a bright pixel bleeds, in screen pixels.
pub struct Bloom {
    radius: f32,
    threshold: f32,
}

impl Bloom {
    /// Blur the bright parts of the frame and add them back over the original.
    /// Two passes (horizontal then vertical) so the cost stays linear in radius.
    pub fn apply(&self, batch: &mut Batch, viewport: Vec2, strength: f32) {
        let r = self.radius * strength.clamp(0.0, 1.0);
        batch.blur_horizontal(r, self.threshold);
        batch.blur_vertical(r, self.threshold);
    }
}
