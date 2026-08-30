// Contract test for astar_bool_grid_8. FROZEN: a solver must not modify this file.
#[path = "pathfind.rs"]
mod pathfind;
use pathfind::{astar_bool_grid, astar_bool_grid_8};

/// An open grid with no blocked cells.
fn open(w: u32, h: u32) -> Vec<bool> {
    vec![false; (w * h) as usize]
}

#[test]
fn start_equals_goal_is_an_empty_path() {
    let g = open(4, 4);
    assert_eq!(astar_bool_grid_8(4, 4, &g, (1, 1), (1, 1)), Some(Vec::new()));
}

#[test]
fn a_straight_line_is_unchanged_by_diagonals() {
    // Purely horizontal: diagonal moves cannot beat 3 steps.
    let g = open(5, 5);
    let path = astar_bool_grid_8(5, 5, &g, (0, 0), (3, 0)).expect("reachable");
    assert_eq!(path.len(), 3);
    assert_eq!(*path.last().unwrap(), (3, 0));
}

#[test]
fn a_diagonal_run_is_shorter_than_the_4_connected_one() {
    // (0,0) -> (3,3). 4-connected needs 6 steps; 8-connected needs 3.
    let g = open(5, 5);
    let four = astar_bool_grid(5, 5, &g, (0, 0), (3, 3)).expect("reachable");
    let eight = astar_bool_grid_8(5, 5, &g, (0, 0), (3, 3)).expect("reachable");
    assert_eq!(four.len(), 6, "4-connected baseline");
    assert_eq!(eight.len(), 3, "8-connected should cut the corner");
    assert_eq!(*eight.last().unwrap(), (3, 3));
}

#[test]
fn the_path_excludes_the_start_and_includes_the_goal() {
    let g = open(4, 4);
    let path = astar_bool_grid_8(4, 4, &g, (0, 0), (2, 2)).expect("reachable");
    assert!(!path.contains(&(0, 0)), "start is excluded: {path:?}");
    assert_eq!(*path.last().unwrap(), (2, 2));
}

#[test]
fn every_step_moves_at_most_one_cell_in_each_axis() {
    let g = open(6, 6);
    let path = astar_bool_grid_8(6, 6, &g, (0, 0), (5, 5)).expect("reachable");
    let mut prev = (0i32, 0i32);
    for &(c, r) in &path {
        let (c, r) = (c as i32, r as i32);
        let (dc, dr) = ((c - prev.0).abs(), (r - prev.1).abs());
        assert!(dc <= 1 && dr <= 1, "jumped from {prev:?} to {:?}", (c, r));
        assert!(dc + dr > 0, "stood still at {:?}", (c, r));
        prev = (c, r);
    }
}

#[test]
fn a_blocked_goal_is_unreachable() {
    let mut g = open(4, 4);
    g[2 * 4 + 2] = true; // (2,2) blocked
    assert_eq!(astar_bool_grid_8(4, 4, &g, (0, 0), (2, 2)), None);
}

#[test]
fn it_routes_around_a_wall() {
    // A vertical wall across column 2, with a gap at the bottom row.
    let (w, h) = (5u32, 5u32);
    let mut g = open(w, h);
    for r in 0..4 {
        g[(r * w + 2) as usize] = true;
    }
    let path = astar_bool_grid_8(w, h, &g, (0, 0), (4, 0)).expect("gap at row 4");
    // It must actually get there without stepping on a blocked cell.
    assert_eq!(*path.last().unwrap(), (4, 0));
    for &(c, r) in &path {
        assert!(!g[(r as u32 * w + c as u32) as usize], "stepped on a wall at {:?}", (c, r));
    }
}

#[test]
fn an_enclosed_goal_is_unreachable() {
    // Goal fully walled off, including the diagonals -- an 8-connected search
    // must not squeeze through a corner that a 4-connected one could not.
    let (w, h) = (5u32, 5u32);
    let mut g = open(w, h);
    for (c, r) in [(3, 0), (3, 1), (3, 2), (4, 2)] {
        g[(r * w + c) as usize] = true;
    }
    assert_eq!(astar_bool_grid_8(w, h, &g, (0, 0), (4, 0)), None);
}
