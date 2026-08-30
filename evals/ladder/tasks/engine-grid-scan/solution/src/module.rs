//! A ship module — a small authored tile patch that gets stamped into a hull.
//!
//! Like `Floor` it owns a decoded `TileGrid<TileKind>` cache, but it was written
//! later and never grew the scan helpers `Floor` has.

use crate::tilegrid::TileGrid;
use crate::TileKind;

pub struct Module {
    grid: TileGrid<TileKind>,
}

impl Module {
    pub fn new(w: u32, h: u32, tiles: Vec<TileKind>) -> Self {
        assert_eq!(tiles.len(), (w * h) as usize, "tile count must match dims");
        let mut grid = TileGrid::new_filled(w, h, TileKind::Empty);
        for (i, t) in tiles.into_iter().enumerate() {
            let (c, r) = ((i as u32 % w) as i32, (i as u32 / w) as i32);
            grid.set(c, r, t);
        }
        Module { grid }
    }

    pub fn width(&self) -> u32 {
        self.grid.width()
    }

    pub fn height(&self) -> u32 {
        self.grid.height()
    }

    /// `TileGrid::tile_at` takes `i32`, so we cast internally.
    pub fn tile_at(&self, col: u32, row: u32) -> TileKind {
        self.grid.tile_at(col as i32, row as i32)
    }

    /// First tile of the given `kind` in row-major scan order, as `(col, row)`.
    ///
    /// A module never had this; it is the same grid scan the station floor uses,
    /// so both now share one implementation.
    pub fn find_first(&self, kind: TileKind) -> Option<(u32, u32)> {
        self.grid.find_first(kind)
    }

    /// Group contiguous tiles of one `kind` into rectangular regions.
    pub fn tile_regions(&self, kind: TileKind) -> Vec<([u32; 2], [u32; 2])> {
        self.grid.regions(kind)
    }
}
