//! Generic grid A* pathfinding primitives.
//!
//! [`astar_bool_grid`] takes a `&[bool]` blocking mask and finds a
//! 4-connected path between two cells. Manhattan heuristic, uniform cost
//! per step. Returned path excludes the start cell and includes the goal,
//! in `(col, row)` pairs. Empty `Vec` when start == goal. `None` when
//! unreachable.
//!
//! Game-side grid pathfinders (tile floors with per-tile blocking rules,
//! ACL doors, etc.) live in `void_sim::pathfind` and reuse this
//! primitive when the tile source can be flattened to a bool grid.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

/// A* open set: a min-heap keyed on `(f_score, g_score, (col, row))`.
/// `Reverse` turns `BinaryHeap`'s max-heap into the min-heap A* wants,
/// and `g_score` breaks f-score ties toward the deeper node.
type OpenSet = BinaryHeap<Reverse<(u32, u32, (i32, i32))>>;

/// A caller-supplied tile-blocking source. Lets `astar_grid` run over
/// any game-side tile representation (station floor, ship exterior,
/// etc.) without engine dragging in game types. Out-of-bounds cells
/// should return `true` from `blocks`.
pub trait TileSource {
    /// Grid dimensions in `(width, height)` tile counts.
    fn dims(&self) -> (u32, u32);
    /// True if `(c, r)` is impassable. Return `true` for out-of-bounds.
    fn blocks(&self, c: i32, r: i32) -> bool;
}

/// A* on any [`TileSource`]. Same rules as [`astar_bool_grid`] —
/// 4-connected, Manhattan heuristic, uniform cost, excludes start,
/// includes goal. `extra_blocked` layers extra impassable cells on top
/// (used to reject e.g. occupied dock pads that the tile source itself
/// would call walkable).
pub fn astar_tile_grid<T: TileSource>(
    src: &T,
    start: (i32, i32),
    goal:  (i32, i32),
    extra_blocked: &HashSet<(i32, i32)>,
) -> Option<Vec<(u16, u16)>> {
    if start == goal { return Some(Vec::new()); }
    if src.blocks(goal.0, goal.1) { return None; }
    let (w, h) = src.dims();
    let heur = |p: (i32, i32)| -> u32 {
        (p.0 - goal.0).unsigned_abs() + (p.1 - goal.1).unsigned_abs()
    };
    let mut open: OpenSet = BinaryHeap::new();
    let mut g_score: HashMap<(i32, i32), u32> = HashMap::new();
    let mut came_from: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    let mut closed: HashSet<(i32, i32)> = HashSet::new();
    open.push(Reverse((heur(start), 0, start)));
    g_score.insert(start, 0);
    let cap = (w as usize).saturating_mul(h as usize).max(1);
    let mut expanded = 0usize;
    while let Some(Reverse((_f, g, cur))) = open.pop() {
        if cur == goal { return Some(reconstruct(&came_from, cur)); }
        if !closed.insert(cur) { continue; }
        expanded += 1;
        if expanded > cap { return None; }
        for (dx, dy) in [(0i32, 1i32), (0, -1), (1, 0), (-1, 0)] {
            let nb = (cur.0 + dx, cur.1 + dy);
            if src.blocks(nb.0, nb.1) { continue; }
            if extra_blocked.contains(&nb) { continue; }
            if closed.contains(&nb) { continue; }
            let tentative = g + 1;
            let better = g_score.get(&nb).is_none_or(|&old| tentative < old);
            if !better { continue; }
            g_score.insert(nb, tentative);
            came_from.insert(nb, cur);
            open.push(Reverse((tentative + heur(nb), tentative, nb)));
        }
    }
    None
}

/// [`astar_tile_grid`] but **8-connected**: the four orthogonal steps plus
/// the four diagonals, uniform cost per step.
///
/// Same conventions — excludes the start, includes the goal, empty `Vec`
/// when start == goal, `None` when unreachable, `extra_blocked` layered on
/// top of the tile source.
///
/// Because a diagonal costs the same as an orthogonal step, the admissible
/// heuristic is Chebyshev distance, not Manhattan: Manhattan over-estimates
/// a diagonal run and would let A* return a non-optimal path. A diagonal is
/// only taken when both adjacent orthogonal cells are clear, so a unit never
/// squeezes through the seam between two touching wall corners.
pub fn astar_tile_grid_8<T: TileSource>(
    src: &T,
    start: (i32, i32),
    goal:  (i32, i32),
    extra_blocked: &HashSet<(i32, i32)>,
) -> Option<Vec<(u16, u16)>> {
    if start == goal { return Some(Vec::new()); }
    if src.blocks(goal.0, goal.1) { return None; }
    if extra_blocked.contains(&goal) { return None; }
    let (w, h) = src.dims();
    let heur = |p: (i32, i32)| -> u32 {
        (p.0 - goal.0).unsigned_abs().max((p.1 - goal.1).unsigned_abs())
    };
    let blocked = |c: i32, r: i32| -> bool {
        src.blocks(c, r) || extra_blocked.contains(&(c, r))
    };
    let mut open: OpenSet = BinaryHeap::new();
    let mut g_score: HashMap<(i32, i32), u32> = HashMap::new();
    let mut came_from: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    let mut closed: HashSet<(i32, i32)> = HashSet::new();
    open.push(Reverse((heur(start), 0, start)));
    g_score.insert(start, 0);
    let cap = (w as usize).saturating_mul(h as usize).max(1);
    let mut expanded = 0usize;
    while let Some(Reverse((_f, g, cur))) = open.pop() {
        if cur == goal { return Some(reconstruct(&came_from, cur)); }
        if !closed.insert(cur) { continue; }
        expanded += 1;
        if expanded > cap { return None; }
        for (dx, dy) in [
            (0i32, 1i32), (0, -1), (1, 0), (-1, 0),
            (1, 1), (1, -1), (-1, 1), (-1, -1),
        ] {
            let nb = (cur.0 + dx, cur.1 + dy);
            if blocked(nb.0, nb.1) { continue; }
            // No corner-cutting: both orthogonal neighbours must be clear.
            if dx != 0 && dy != 0
                && (blocked(cur.0 + dx, cur.1) || blocked(cur.0, cur.1 + dy))
            {
                continue;
            }
            if closed.contains(&nb) { continue; }
            let tentative = g + 1;
            let better = g_score.get(&nb).is_none_or(|&old| tentative < old);
            if !better { continue; }
            g_score.insert(nb, tentative);
            came_from.insert(nb, cur);
            open.push(Reverse((tentative + heur(nb), tentative, nb)));
        }
    }
    None
}

/// A* over a caller-supplied bool grid (`blocked[row*width + col]`).
/// 4-connected, Manhattan heuristic. Returned path excludes the start
/// cell and includes the goal, in `(col, row)` pairs.
pub fn astar_bool_grid(
    width:   u32,
    height:  u32,
    blocked: &[bool],
    start:   (i32, i32),
    goal:    (i32, i32),
) -> Option<Vec<(u16, u16)>> {
    if start == goal { return Some(Vec::new()); }
    let w = width as i32;
    let h = height as i32;
    let idx = |c: i32, r: i32| -> Option<usize> {
        if c < 0 || r < 0 || c >= w || r >= h { return None; }
        Some((r as usize) * (width as usize) + (c as usize))
    };
    let blocks = |c: i32, r: i32| -> bool {
        match idx(c, r) {
            Some(i) => blocked.get(i).copied().unwrap_or(true),
            None    => true,
        }
    };
    if blocks(goal.0, goal.1) { return None; }
    let heur = |p: (i32, i32)| -> u32 {
        (p.0 - goal.0).unsigned_abs() + (p.1 - goal.1).unsigned_abs()
    };
    let mut open: OpenSet = BinaryHeap::new();
    let mut g_score: HashMap<(i32, i32), u32> = HashMap::new();
    let mut came_from: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    let mut closed: HashSet<(i32, i32)> = HashSet::new();
    open.push(Reverse((heur(start), 0, start)));
    g_score.insert(start, 0);
    let cap = (w as usize).saturating_mul(h as usize).max(1);
    let mut expanded = 0usize;
    while let Some(Reverse((_f, g, cur))) = open.pop() {
        if cur == goal { return Some(reconstruct(&came_from, cur)); }
        if !closed.insert(cur) { continue; }
        expanded += 1;
        if expanded > cap { return None; }
        for (dx, dy) in [(0i32, 1i32), (0, -1), (1, 0), (-1, 0)] {
            let nb = (cur.0 + dx, cur.1 + dy);
            if blocks(nb.0, nb.1) { continue; }
            if closed.contains(&nb) { continue; }
            let tentative = g + 1;
            let better = g_score.get(&nb).is_none_or(|&old| tentative < old);
            if !better { continue; }
            g_score.insert(nb, tentative);
            came_from.insert(nb, cur);
            open.push(Reverse((tentative + heur(nb), tentative, nb)));
        }
    }
    None
}

fn reconstruct(
    came_from: &HashMap<(i32, i32), (i32, i32)>,
    mut cur: (i32, i32),
) -> Vec<(u16, u16)> {
    let mut path = Vec::new();
    while let Some(&prev) = came_from.get(&cur) {
        path.push((cur.0 as u16, cur.1 as u16));
        cur = prev;
    }
    path.reverse();
    path
}
