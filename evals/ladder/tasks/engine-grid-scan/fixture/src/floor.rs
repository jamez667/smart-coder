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

    /// Group contiguous tiles of one `kind` into rectangular regions via a
    /// 4-connected flood-fill, returning each region's top-left anchor and tile
    /// size. The single grouping primitive behind every multi-tile derivation —
    /// elevator cars, apartment doors, dock-pad synthesis.
    pub fn tile_regions(&self, kind: TileKind) -> Vec<([u32; 2], [u32; 2])> {
        let w = self.width() as i32;
        let h = self.height() as i32;
        let mut visited = vec![false; (w.max(0) * h.max(0)) as usize];
        let idx = |c: i32, r: i32| (r * w + c) as usize;
        let mut out = Vec::new();
        for r0 in 0..h {
            for c0 in 0..w {
                if visited[idx(c0, r0)] || self.tile_at(c0, r0) != kind {
                    continue;
                }
                let mut stack = vec![(c0, r0)];
                let (mut min_c, mut max_c, mut min_r, mut max_r) = (c0, c0, r0, r0);
                while let Some((c, r)) = stack.pop() {
                    if c < 0 || r < 0 || c >= w || r >= h || visited[idx(c, r)] {
                        continue;
                    }
                    if self.tile_at(c, r) != kind {
                        continue;
                    }
                    visited[idx(c, r)] = true;
                    min_c = min_c.min(c);
                    max_c = max_c.max(c);
                    min_r = min_r.min(r);
                    max_r = max_r.max(r);
                    stack.push((c + 1, r));
                    stack.push((c - 1, r));
                    stack.push((c, r + 1));
                    stack.push((c, r - 1));
                }
                out.push((
                    [min_c as u32, min_r as u32],
                    [(max_c - min_c + 1) as u32, (max_r - min_r + 1) as u32],
                ));
            }
        }
        out
    }

    /// First tile of the given `kind` in row-major scan order, as `(col, row)`.
    /// `None` if the floor has no such tile.
    pub fn find_first(&self, kind: TileKind) -> Option<(u32, u32)> {
        for row in 0..self.height() {
            for col in 0..self.width() {
                if self.tile_at(col as i32, row as i32) == kind {
                    return Some((col, row));
                }
            }
        }
        None
    }
}
