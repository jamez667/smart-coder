// Contract test for the shared grid scan. FROZEN: a solver must not modify this file.
use void_grid_task::floor::Floor;
use void_grid_task::module::Module;
use void_grid_task::tilegrid::TileGrid;
use void_grid_task::TileKind;

fn tiles(rows: &[&str]) -> (u32, u32, Vec<TileKind>) {
    let h = rows.len() as u32;
    let w = rows[0].len() as u32;
    let mut out = Vec::new();
    for r in rows {
        assert_eq!(r.len() as u32, w, "rows must be equal length");
        for ch in r.chars() {
            out.push(match ch {
                '.' => TileKind::Floor,
                '#' => TileKind::Wall,
                'p' => TileKind::Pad,
                's' => TileKind::PlayerSpawn,
                'E' => TileKind::Elevator,
                _ => TileKind::Empty,
            });
        }
    }
    (w, h, out)
}

// ---- the engine primitive -------------------------------------------------

#[test]
fn grid_find_first_scans_row_major() {
    let mut g: TileGrid<TileKind> = TileGrid::new_filled(3, 3, TileKind::Empty);
    g.set(2, 0, TileKind::PlayerSpawn);
    g.set(0, 2, TileKind::PlayerSpawn);
    // Row-major: (2,0) comes before (0,2).
    assert_eq!(g.find_first(TileKind::PlayerSpawn), Some((2, 0)));
    assert_eq!(g.find_first(TileKind::Pad), None);
}

#[test]
fn grid_regions_groups_contiguous_runs() {
    let (w, h, t) = tiles(&["EE.E", "....", "E..E"]);
    let mut g: TileGrid<TileKind> = TileGrid::new_filled(w, h, TileKind::Empty);
    for (i, k) in t.iter().enumerate() {
        g.set((i as u32 % w) as i32, (i as u32 / w) as i32, *k);
    }
    let mut regions = g.regions(TileKind::Elevator);
    regions.sort();
    // Four separate cars: the pair at (0,0)-(1,0), and three singles.
    assert_eq!(regions.len(), 4, "got {regions:?}");
    assert!(regions.contains(&([0, 0], [2, 1])), "the 2-wide run: {regions:?}");
    assert!(regions.contains(&([3, 0], [1, 1])), "{regions:?}");
    assert!(regions.contains(&([0, 2], [1, 1])), "{regions:?}");
    assert!(regions.contains(&([3, 2], [1, 1])), "{regions:?}");
}

#[test]
fn grid_regions_is_4_connected_not_8() {
    // Two tiles touching only at a corner are SEPARATE regions.
    let (w, h, t) = tiles(&["E.", ".E"]);
    let mut g: TileGrid<TileKind> = TileGrid::new_filled(w, h, TileKind::Empty);
    for (i, k) in t.iter().enumerate() {
        g.set((i as u32 % w) as i32, (i as u32 / w) as i32, *k);
    }
    assert_eq!(g.regions(TileKind::Elevator).len(), 2);
}

#[test]
fn grid_regions_spans_an_l_shape_as_one_bounding_box() {
    let (w, h, t) = tiles(&["EE.", "E..", "..."]);
    let mut g: TileGrid<TileKind> = TileGrid::new_filled(w, h, TileKind::Empty);
    for (i, k) in t.iter().enumerate() {
        g.set((i as u32 % w) as i32, (i as u32 / w) as i32, *k);
    }
    // One connected L, reported as its bounding box.
    assert_eq!(g.regions(TileKind::Elevator), vec![([0, 0], [2, 2])]);
}

#[test]
fn grid_scans_are_empty_on_an_empty_grid() {
    let g: TileGrid<TileKind> = TileGrid::empty();
    assert_eq!(g.find_first(TileKind::Floor), None);
    assert_eq!(g.regions(TileKind::Floor), Vec::new());
}

// ---- consumer one: Floor keeps its behaviour ------------------------------

#[test]
fn floor_find_first_still_works() {
    let (w, h, t) = tiles(&["...", ".s.", "..s"]);
    let f = Floor::new(w, h, t);
    assert_eq!(f.find_first(TileKind::PlayerSpawn), Some((1, 1)));
}

#[test]
fn floor_tile_regions_still_works() {
    let (w, h, t) = tiles(&["EE.", "...", "..E"]);
    let f = Floor::new(w, h, t);
    let mut got = f.tile_regions(TileKind::Elevator);
    got.sort();
    assert_eq!(got, vec![([0, 0], [2, 1]), ([2, 2], [1, 1])]);
}

// ---- consumer two: Module gains the same capability ------------------------

#[test]
fn module_can_find_a_tile() {
    let (w, h, t) = tiles(&["##p", "...", "p.."]);
    let m = Module::new(w, h, t);
    assert_eq!(m.find_first(TileKind::Pad), Some((2, 0)));
    assert_eq!(m.find_first(TileKind::PlayerSpawn), None);
}

#[test]
fn module_can_group_regions() {
    let (w, h, t) = tiles(&["pp.", "...", ".pp"]);
    let m = Module::new(w, h, t);
    let mut got = m.tile_regions(TileKind::Pad);
    got.sort();
    assert_eq!(got, vec![([0, 0], [2, 1]), ([1, 2], [2, 1])]);
}
