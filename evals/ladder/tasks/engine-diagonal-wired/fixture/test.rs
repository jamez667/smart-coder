// Contract test for the 8-connected exterior pathfinder. FROZEN: do not modify.
#[path = "nav.rs"]
mod nav;

use nav::pathfind::astar_tile_grid_8;
use nav::{astar_grid_exterior, astar_grid_exterior_8, Floor, TileKind};
use std::collections::HashSet;

/// An exterior floor of open vacuum, with the given cells walled.
fn vacuum(w: u32, h: u32, walls: &[(u32, u32)]) -> Floor {
    let mut tiles = vec![TileKind::Empty; (w * h) as usize];
    for &(c, r) in walls {
        tiles[(r * w + c) as usize] = TileKind::Wall;
    }
    Floor::new(w, h, tiles)
}

// ---- the engine primitive ------------------------------------------------

/// A minimal TileSource so the engine function is testable on its own.
struct Open(u32, u32);
impl nav::pathfind::TileSource for Open {
    fn dims(&self) -> (u32, u32) { (self.0, self.1) }
    fn blocks(&self, c: i32, r: i32) -> bool {
        c < 0 || r < 0 || c >= self.0 as i32 || r >= self.1 as i32
    }
}

#[test]
fn engine_start_equals_goal_is_empty() {
    let s = Open(4, 4);
    assert_eq!(astar_tile_grid_8(&s, (1, 1), (1, 1), &HashSet::new()), Some(Vec::new()));
}

#[test]
fn engine_cuts_the_corner() {
    // (0,0) -> (3,3) is 3 diagonal steps, not 6 orthogonal ones.
    let s = Open(5, 5);
    let p = astar_tile_grid_8(&s, (0, 0), (3, 3), &HashSet::new()).expect("reachable");
    assert_eq!(p.len(), 3);
    assert_eq!(*p.last().unwrap(), (3, 3));
}

#[test]
fn engine_respects_extra_blocked() {
    let s = Open(4, 4);
    let extra: HashSet<(i32, i32)> = [(2, 3), (3, 2), (2, 2)].into_iter().collect();
    assert_eq!(astar_tile_grid_8(&s, (0, 0), (3, 3), &extra), None);
}

// ---- the game-side wrapper ----------------------------------------------

#[test]
fn game_wrapper_beats_the_4_connected_one_diagonally() {
    let f = vacuum(6, 6, &[]);
    let four = astar_grid_exterior(&f, (0, 0), (4, 4), &HashSet::new()).expect("reachable");
    let eight = astar_grid_exterior_8(&f, (0, 0), (4, 4), &HashSet::new()).expect("reachable");
    assert_eq!(four.len(), 8, "4-connected baseline");
    assert_eq!(eight.len(), 4, "8-connected cuts the corner");
    assert_eq!(*eight.last().unwrap(), (4, 4));
}

#[test]
fn game_wrapper_still_clamps_an_out_of_bounds_start() {
    // The ship is far outside the exterior grid; the search must clamp rather
    // than fail, exactly as the 4-connected wrapper does.
    let f = vacuum(5, 5, &[]);
    let p = astar_grid_exterior_8(&f, (-40, -40), (2, 2), &HashSet::new()).expect("clamped");
    assert_eq!(*p.last().unwrap(), (2, 2));
}

#[test]
fn game_wrapper_treats_walls_as_solid() {
    // A wall across column 2 with a gap at the bottom row.
    let walls: Vec<(u32, u32)> = (0..4).map(|r| (2u32, r)).collect();
    let f = vacuum(5, 5, &walls);
    let p = astar_grid_exterior_8(&f, (0, 0), (4, 0), &HashSet::new()).expect("gap at row 4");
    assert_eq!(*p.last().unwrap(), (4, 0));
    for &(c, r) in &p {
        assert_ne!(f.tile_at(c as i32, r as i32), TileKind::Wall, "walked into a wall");
    }
}

#[test]
fn game_wrapper_will_not_squeeze_between_two_wall_corners() {
    // Walls at (1,0) and (0,1) leave (0,0) and (1,1) touching only at a corner.
    // A unit has width; it must not slip diagonally through that seam.
    let f = vacuum(3, 3, &[(1, 0), (0, 1)]);
    assert_eq!(astar_grid_exterior_8(&f, (0, 0), (1, 1), &HashSet::new()), None);
}

#[test]
fn the_4_connected_wrapper_is_unchanged() {
    // Adding the new path must not alter the old one: still orthogonal-only.
    let f = vacuum(4, 4, &[]);
    let p = astar_grid_exterior(&f, (0, 0), (2, 2), &HashSet::new()).expect("reachable");
    assert_eq!(p.len(), 4, "4-connected must still cost 4 steps");
}
