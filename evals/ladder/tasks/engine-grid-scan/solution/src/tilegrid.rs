//! Generic 2D tile grid — a flat `Vec<T>` sized `w*h` with cached
//! dimensions and the "grid centered on origin, row 0 = top" geometry
//! that the ships/stations in this project all share.
//!
//! Extracted from `void_sim::station_interior::Floor` (and the
//! `void_sim::module::Module` inner grid, which shares the same shape).
//! Callers that already stored `grid: Vec<T>`, `cached_w`, `cached_h`
//! now delegate the arithmetic here and keep only their game overlays
//! (apartment ownership, ACL doors, module composites, ...).
//!
//! Coord conventions:
//! - `w` / `h` in tiles are `u32` (never negative, and the grid backing
//!   store is sized from them).
//! - Column / row reads take `i32` so callers can pass unclamped
//!   values from the collision / pathfinding hot paths without a
//!   pre-check. Out-of-bounds `tile_at` returns `T::default()` — for
//!   `TileKind` that's `Empty`, which is what every caller already
//!   treated an off-grid read as.
//! - `set` is bounds-checked and silently no-ops on out-of-range
//!   coords, matching `Floor::set_tile`.

use glam::DVec2;

pub use crate::tile_collide::{pos_to_tile_default, tile_center_default};

/// Owned tile grid + cached dimensions. `T` is the per-tile value —
/// usually an enum like `TileKind` — and needs to be `Copy + Default`
/// so out-of-bounds reads can synthesise a safe fill without touching
/// the backing store.
#[derive(Clone, Debug, Default)]
pub struct TileGrid<T: Copy + Default> {
    grid: Vec<T>,
    w: u32,
    h: u32,
}

impl<T: Copy + Default> TileGrid<T> {
    /// Empty grid — no storage, `dims() == (0, 0)`. Callers rebuild it
    /// from source data via [`TileGrid::rebuild_from_rows`] or by
    /// filling with [`TileGrid::new_filled`].
    pub fn empty() -> Self { Self { grid: Vec::new(), w: 0, h: 0 } }

    /// Allocate `w*h` tiles all initialised to `val`. `T: Default` is
    /// only required at the type level; `val` can be any value.
    pub fn new_filled(w: u32, h: u32, val: T) -> Self {
        Self { grid: vec![val; (w * h) as usize], w, h }
    }

    /// (Width, height) in tiles.
    #[inline]
    pub fn dims(&self) -> (u32, u32) { (self.w, self.h) }

    /// Width in tiles.
    #[inline]
    pub fn width(&self) -> u32 { self.w }

    /// Height in tiles.
    #[inline]
    pub fn height(&self) -> u32 { self.h }

    /// True when `dims()` is `(0, 0)`. Cheap sentinel used by fixture
    /// code paths that construct a value via `..Default::default()`
    /// and never call `rebuild_from_rows`.
    #[inline]
    pub fn is_empty(&self) -> bool { self.grid.is_empty() }

    /// Read at `(col, row)`. Out-of-bounds (including negative) reads
    /// return `T::default()` so callers can walk past the edge without
    /// a pre-check — collision, pathfinding, and neighbour scans rely
    /// on this.
    #[inline]
    pub fn tile_at(&self, col: i32, row: i32) -> T {
        if col < 0 || row < 0 || self.grid.is_empty() { return T::default(); }
        let (c, r) = (col as u32, row as u32);
        if c >= self.w || r >= self.h { return T::default(); }
        self.grid[(r * self.w + c) as usize]
    }

    /// True if `(col, row)` is inside the grid bounds.
    #[inline]
    pub fn in_bounds(&self, col: i32, row: i32) -> bool {
        col >= 0 && row >= 0 && (col as u32) < self.w && (row as u32) < self.h
    }

    /// Write at `(col, row)`. Silently no-ops when out of bounds; the
    /// editor's `set_tile` mutator relies on this to be safe against
    /// stray clicks past the grid edge.
    #[inline]
    pub fn set(&mut self, col: i32, row: i32, val: T) {
        if !self.in_bounds(col, row) || self.grid.is_empty() { return; }
        let idx = (row as u32 * self.w + col as u32) as usize;
        self.grid[idx] = val;
    }

    /// Fill every tile with `val`, keeping the current dimensions.
    pub fn fill(&mut self, val: T) {
        for c in self.grid.iter_mut() { *c = val; }
    }

    /// Borrow the flat backing buffer, row-major `r*w + c`.
    #[inline]
    pub fn as_slice(&self) -> &[T] { &self.grid }

    /// Iterate every cell as `(col, row, val)` in row-major scan order.
    pub fn iter_cells(&self) -> impl Iterator<Item = (u32, u32, T)> + '_ {
        let w = self.w;
        self.grid.iter().enumerate().map(move |(i, &v)| {
            let i = i as u32;
            (i % w, i / w, v)
        })
    }

    /// Rebuild storage from a row-major provider. `w` / `h` are set to
    /// the passed dims and every cell is filled by calling `read(c, r)`.
    /// The standard use is decoding a `Vec<String>` of glyphs into
    /// enum tiles — pass a closure that indexes into the strings, or
    /// prefer [`TileGrid::rebuild_from_glyphs`] which handles that
    /// pattern directly.
    pub fn rebuild_from_rows<F>(&mut self, w: u32, h: u32, mut read: F)
    where
        F: FnMut(u32, u32) -> T,
    {
        self.w = w;
        self.h = h;
        let mut buf = Vec::with_capacity((w * h) as usize);
        for r in 0..h {
            for c in 0..w {
                buf.push(read(c, r));
            }
        }
        self.grid = buf;
    }

    /// Rebuild from a `&[String]` of glyph rows via a `char -> T`
    /// decoder. Dims are derived from the string vec (height = row
    /// count, width = first row's byte length). Bytes past a short
    /// row and unmapped chars fall through to `T::default()`. Every
    /// tile-file loader in the project (station floors, ship
    /// interiors, module tiles) shares this exact decode path.
    pub fn rebuild_from_glyphs<F>(&mut self, tiles: &[String], mut decode: F)
    where
        F: FnMut(char) -> T,
    {
        let h = tiles.len() as u32;
        let w = tiles.first().map(|r| r.len() as u32).unwrap_or(0);
        self.rebuild_from_rows(w, h, |c, r| {
            tiles.get(r as usize)
                .and_then(|row| row.as_bytes().get(c as usize).copied())
                .map(|b| decode(b as char))
                .unwrap_or_default()
        });
    }

    /// Read a `(col, row)` from a `&[String]` of glyph rows without a
    /// prebuilt grid. Returns `T::default()` for out-of-range or short
    /// rows. Used by the `tile_at` fallback path on fresh-from-`serde`
    /// instances where `rebuild_from_glyphs` hasn't run yet.
    pub fn tile_at_glyphs<F>(tiles: &[String], col: i32, row: i32, mut decode: F) -> T
    where
        F: FnMut(char) -> T,
    {
        if col < 0 || row < 0 { return T::default(); }
        tiles.get(row as usize)
            .and_then(|s| s.as_bytes().get(col as usize).copied())
            .map(|b| decode(b as char))
            .unwrap_or_default()
    }

    /// World-space centre of tile `(col, row)`. Standard "grid centered
    /// on origin, row 0 at top" layout. Forwards to
    /// [`tile_center_default`] — kept as a method so callers reach it
    /// through the grid struct without a separate import.
    #[inline]
    pub fn tile_center(&self, col: u32, row: u32, tile_size_m: f32) -> DVec2 {
        tile_center_default(col as i32, row as i32, self.w, self.h, tile_size_m)
    }

    /// World-space position → `(col, row)`. Inverse of
    /// [`TileGrid::tile_center`]. Caller should range-check.
    #[inline]
    pub fn pos_to_tile(&self, pos: DVec2, tile_size_m: f32) -> (i32, i32) {
        pos_to_tile_default(pos, self.w, self.h, tile_size_m)
    }
}

/// Rotate a row-major `w * h` tile buffer by `rot` quarter-turns
/// clockwise (taken modulo 4), returning `(new_w, new_h, tiles)`.
///
/// Odd quarter-turns transpose the dimensions. `rot == 0` is a straight
/// copy. Reads go through `src`, a `(col, row) -> T` closure, rather
/// than a slice, so callers whose canonical storage is glyph rows or a
/// lazily-populated cache can rotate without materialising a flat buffer
/// first.
///
/// Extracted because `void_sim::module::Module::rotated_tiles` and the
/// tilemap editor's `Clipboard::rotated` carried byte-identical copies
/// of this kernel, and a divergence between them would silently desync
/// the editor preview from the runtime composite.
pub fn rotate_tiles<T, F>(w: u32, h: u32, rot: u8, mut src: F) -> (u32, u32, Vec<T>)
where
    T: Copy + Default,
    F: FnMut(u32, u32) -> T,
{
    let rot = rot % 4;
    let (sw, sh) = (w as usize, h as usize);
    let (nw, nh) = match rot {
        1 | 3 => (sh, sw),
        _     => (sw, sh),
    };
    let mut out = vec![T::default(); nw * nh];
    for r in 0..sh {
        for c in 0..sw {
            let val = src(c as u32, r as u32);
            let (nc, nr) = match rot {
                0 => (c, r),
                1 => (sh - 1 - r, c),          // 90° CW
                2 => (sw - 1 - c, sh - 1 - r), // 180°
                3 => (r, sw - 1 - c),          // 270° CW (= 90° CCW)
                _ => unreachable!(),
            };
            out[nr * nw + nc] = val;
        }
    }
    (nw as u32, nh as u32, out)
}

impl<T: Copy + Default + PartialEq> TileGrid<T> {
    /// First cell equal to `val` in row-major scan order, as `(col, row)`.
    /// `None` when the grid holds no such cell.
    ///
    /// Lifted here from the station floor, which had hand-rolled it: it is a
    /// property of a grid, not of a station, and the ship-module grid needs the
    /// same thing.
    pub fn find_first(&self, val: T) -> Option<(u32, u32)> {
        for row in 0..self.height() {
            for col in 0..self.width() {
                if self.tile_at(col as i32, row as i32) == val {
                    return Some((col, row));
                }
            }
        }
        None
    }

    /// Group contiguous cells equal to `val` into rectangular regions via a
    /// 4-connected flood-fill, returning each region's top-left `anchor` and
    /// `size` in cells.
    ///
    /// 4-connected on purpose: two cells touching only at a corner are separate
    /// regions, which is what "one elevator car" or "one dock pad" means. An
    /// L-shaped run is reported as its bounding box, so `size` may cover cells
    /// that are not themselves `val`.
    pub fn regions(&self, val: T) -> Vec<([u32; 2], [u32; 2])> {
        let w = self.width() as i32;
        let h = self.height() as i32;
        if w <= 0 || h <= 0 {
            return Vec::new();
        }
        let mut visited = vec![false; (w * h) as usize];
        let idx = |c: i32, r: i32| (r * w + c) as usize;
        let mut out = Vec::new();
        for r0 in 0..h {
            for c0 in 0..w {
                if visited[idx(c0, r0)] || self.tile_at(c0, r0) != val {
                    continue;
                }
                let mut stack = vec![(c0, r0)];
                let (mut min_c, mut max_c, mut min_r, mut max_r) = (c0, c0, r0, r0);
                while let Some((c, r)) = stack.pop() {
                    if c < 0 || r < 0 || c >= w || r >= h || visited[idx(c, r)] {
                        continue;
                    }
                    if self.tile_at(c, r) != val {
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
}
