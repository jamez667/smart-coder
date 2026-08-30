//! Station interior floor — the tile map a station's inside is built from.
//!
//! **Hot-path note**: per-frame queries (`tile_at`, `width`, `height`) read from
//! the decoded `grid` cache (a `TileGrid<TileKind>`), not from the authored glyph
//! strings.

use crate::tilegrid::TileGrid;
use crate::TileKind;

pub struct Floor {
    /// Decoded tile cache backing the per-frame `tile_at` lookup.
    grid: TileGrid<TileKind>,
}

impl Floor {
    pub fn new(w: u32, h: u32, tiles: Vec<TileKind>) -> Self {
        assert_eq!(tiles.len(), (w * h) as usize, "tile count must match dims");
        let mut grid = TileGrid::new_filled(w, h, TileKind::Empty);
        for (i, t) in tiles.into_iter().enumerate() {
            let (c, r) = ((i as u32 % w) as i32, (i as u32 / w) as i32);
            grid.set(c, r, t);
        }
        Floor { grid }
    }

    pub fn width(&self) -> u32 {
        self.grid.width()
    }

    pub fn height(&self) -> u32 {
        self.grid.height()
    }

    /// Tile at `(col, row)`; out-of-bounds reads as `Empty`.
    pub fn tile_at(&self, col: i32, row: i32) -> TileKind {
        self.grid.tile_at(col, row)
    }

    /// Group contiguous tiles of one `kind` into rectangular regions.
    ///
    /// The flood-fill itself now lives on `TileGrid` -- it is a property of a
    /// grid, not of a station, and the ship-module grid needed the same thing.
    /// This stays as the name the rest of the station code already calls.
    pub fn tile_regions(&self, kind: TileKind) -> Vec<([u32; 2], [u32; 2])> {
        self.grid.regions(kind)
    }

    /// First tile of the given `kind` in row-major scan order, as `(col, row)`.
    /// `None` if the floor has no such tile.
    pub fn find_first(&self, kind: TileKind) -> Option<(u32, u32)> {
        self.grid.find_first(kind)
    }
}
