//! Grid A* pathfinding for on-foot NPC navigation (game side).
//!
//! 4-connected (N/S/E/W), uniform cost 1 per step, Manhattan heuristic.
//! Operates on a `Floor` — walkable iff `!TileKind::blocks()`.
//! Out-of-bounds tiles read as `Empty` (blocking).
//!
//! Returned path excludes the start tile and includes the goal, in tile
//! coords as `(u16, u16)`. Empty `Vec` when start == goal. `None` when
//! unreachable.
//!
//! The A* kernel itself lives in `pathfind` as the generic
//! `astar_tile_grid` over a `TileSource` trait. This module provides the
//! game-side `TileSource` impls and thin wrappers.

use std::collections::HashSet;

#[path = "pathfind.rs"]
pub mod pathfind;

use pathfind::{astar_tile_grid, TileSource};

/// What a tile is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileKind {
    /// Off-grid interior, or open vacuum on an exterior floor.
    Empty,
    /// Walkable station floor.
    Floor,
    /// Solid.
    Wall,
    /// A docking pad — walkable, but the dock pathfinder may block it.
    Pad,
}

impl TileKind {
    /// True if a unit on foot cannot enter this tile.
    pub fn blocks(self) -> bool {
        !matches!(self, TileKind::Floor | TileKind::Pad)
    }
}

/// A rectangular tile floor.
pub struct Floor {
    w: u32,
    h: u32,
    tiles: Vec<TileKind>,
}

impl Floor {
    pub fn new(w: u32, h: u32, tiles: Vec<TileKind>) -> Self {
        assert_eq!(tiles.len(), (w * h) as usize, "tile count must match dims");
        Floor { w, h, tiles }
    }
    pub fn width(&self) -> u32 { self.w }
    pub fn height(&self) -> u32 { self.h }
    /// Tile at `(c, r)`; out-of-bounds reads as `Empty`.
    pub fn tile_at(&self, c: i32, r: i32) -> TileKind {
        if c < 0 || r < 0 || c >= self.w as i32 || r >= self.h as i32 {
            return TileKind::Empty;
        }
        self.tiles[(r as u32 * self.w + c as u32) as usize]
    }
}

/// Interior-floor `TileSource`: reads `TileKind::blocks()`.
struct InteriorSource<'a>(&'a Floor);

impl<'a> TileSource for InteriorSource<'a> {
    fn dims(&self) -> (u32, u32) { (self.0.width(), self.0.height()) }
    fn blocks(&self, c: i32, r: i32) -> bool { self.0.tile_at(c, r).blocks() }
}

/// Exterior-floor `TileSource`: `Empty` (vacuum) and `Pad` are walkable;
/// everything else is blocking. Out-of-bounds reads block too.
struct ExteriorSource<'a>(&'a Floor);

impl<'a> TileSource for ExteriorSource<'a> {
    fn dims(&self) -> (u32, u32) { (self.0.width(), self.0.height()) }
    fn blocks(&self, c: i32, r: i32) -> bool {
        if c < 0 || r < 0 || c >= self.0.width() as i32 || r >= self.0.height() as i32 {
            return true;
        }
        !matches!(self.0.tile_at(c, r), TileKind::Empty | TileKind::Pad)
    }
}

/// A* on the interior tile grid. See module docs for shape + cost model.
pub fn astar_grid(
    floor: &Floor,
    start: (i32, i32),
    goal:  (i32, i32),
) -> Option<Vec<(u16, u16)>> {
    astar_tile_grid(&InteriorSource(floor), start, goal, &HashSet::new())
}

/// A* over a station's *exterior* tile floor — same shape as the interior
/// pathfinder but with `Empty` re-interpreted as open space (walkable)
/// rather than void (blocking).
///
/// If `start` is outside the floor's grid (the ship may be far from the
/// station while the exterior grid covers only a small box), the search
/// clamps `start` to the nearest in-bounds tile.
pub fn astar_grid_exterior(
    floor: &Floor,
    start: (i32, i32),
    goal:  (i32, i32),
    extra_blocked: &HashSet<(i32, i32)>,
) -> Option<Vec<(u16, u16)>> {
    let clamped = (
        start.0.clamp(0, floor.width()  as i32 - 1),
        start.1.clamp(0, floor.height() as i32 - 1),
    );
    astar_tile_grid(&ExteriorSource(floor), clamped, goal, extra_blocked)
}
