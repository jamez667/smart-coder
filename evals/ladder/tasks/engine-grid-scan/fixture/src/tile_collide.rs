//! World-position <-> tile-index conversion for a grid centred on the origin.
//!
//! Trimmed from `void_engine::tile_collide` to the two functions `tilegrid`
//! re-exports; the walker-push half of that module is not needed here.

use glam::DVec2;

/// World centre of tile `(col, row)` on a grid centred on the origin.
pub fn tile_center_default(col: i32, row: i32, width: u32, height: u32, tile_size_m: f32) -> DVec2 {
    let w = width as f64;
    let h = height as f64;
    let s = tile_size_m as f64;
    DVec2::new(
        (col as f64 - w * 0.5 + 0.5) * s,
        ((h - 1.0) * 0.5 - row as f64) * s,
    )
}

/// Standard "grid centered on origin" tile-index lookup for a world
/// position. Inverse of [`tile_center_default`].
#[inline]
pub fn pos_to_tile_default(pos: DVec2, width: u32, height: u32, tile_size_m: f32) -> (i32, i32) {
    let w = width as f64;
    let h = height as f64;
    let s = tile_size_m as f64;
    let col = (pos.x / s + w * 0.5).floor() as i32;
    let row = ((h - 1.0) * 0.5 - pos.y / s + 0.5).floor() as i32;
    (col, row)
}
