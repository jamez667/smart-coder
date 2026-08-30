// Contract test for the exclusion query. FROZEN: a solver must not modify this file.
use void_ecs_task::entity::EntityId;
use void_ecs_task::world::World;
use void_ecs_task::{physics, render};
use void_ecs_task::{Frozen, Hidden, Position, Sprite, Velocity};

fn at(x: f64, y: f64) -> Position {
    Position { x, y }
}

// ---- the engine primitive -------------------------------------------------

#[test]
fn iter2_without_excludes_the_marked_entity() {
    let mut w = World::new();
    let a = w.spawn();
    w.insert(a, at(0.0, 0.0));
    w.insert(a, Velocity { dx: 1.0, dy: 0.0 });
    let b = w.spawn();
    w.insert(b, at(5.0, 5.0));
    w.insert(b, Velocity { dx: 2.0, dy: 0.0 });
    w.insert(b, Frozen);

    let got: Vec<EntityId> = w
        .iter2_without::<Position, Velocity, Frozen>()
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(got, vec![a], "the frozen entity must be excluded");
}

#[test]
fn iter2_without_yields_everything_when_nothing_is_marked() {
    let mut w = World::new();
    for i in 0..3 {
        let e = w.spawn();
        w.insert(e, at(i as f64, 0.0));
        w.insert(e, Velocity { dx: 1.0, dy: 0.0 });
    }
    let n = w.iter2_without::<Position, Velocity, Frozen>().count();
    assert_eq!(n, 3);
}

#[test]
fn iter2_without_still_requires_both_components() {
    let mut w = World::new();
    let only_pos = w.spawn();
    w.insert(only_pos, at(0.0, 0.0));
    let only_vel = w.spawn();
    w.insert(only_vel, Velocity { dx: 1.0, dy: 1.0 });

    assert_eq!(w.iter2_without::<Position, Velocity, Frozen>().count(), 0);
}

#[test]
fn iter2_without_skips_a_despawned_entity() {
    let mut w = World::new();
    let a = w.spawn();
    w.insert(a, at(0.0, 0.0));
    w.insert(a, Velocity { dx: 1.0, dy: 0.0 });
    let b = w.spawn();
    w.insert(b, at(1.0, 1.0));
    w.insert(b, Velocity { dx: 1.0, dy: 0.0 });
    w.despawn(a);

    let got: Vec<EntityId> = w
        .iter2_without::<Position, Velocity, Frozen>()
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(got, vec![b]);
}

/// The generational-index invariant: a recycled slot must not inherit the old
/// entity's exclusion marker.
#[test]
fn a_recycled_slot_does_not_inherit_the_marker() {
    let mut w = World::new();
    let a = w.spawn();
    w.insert(a, at(0.0, 0.0));
    w.insert(a, Velocity { dx: 1.0, dy: 0.0 });
    w.insert(a, Frozen);
    w.despawn(a);

    // Reuses a's slot with a bumped generation.
    let b = w.spawn();
    assert_eq!(b.index, a.index, "premise: the slot is recycled");
    assert_ne!(b.generation, a.generation);
    w.insert(b, at(9.0, 9.0));
    w.insert(b, Velocity { dx: 1.0, dy: 0.0 });

    let got: Vec<EntityId> = w
        .iter2_without::<Position, Velocity, Frozen>()
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(got, vec![b], "the new entity is not frozen");
}

#[test]
fn the_values_yielded_are_the_live_components() {
    let mut w = World::new();
    let a = w.spawn();
    w.insert(a, at(3.0, 4.0));
    w.insert(a, Velocity { dx: 7.0, dy: 8.0 });

    let got: Vec<(f64, f64, f64, f64)> = w
        .iter2_without::<Position, Velocity, Frozen>()
        .map(|(_, p, v)| (p.x, p.y, v.dx, v.dy))
        .collect();
    assert_eq!(got, vec![(3.0, 4.0, 7.0, 8.0)]);
}

// ---- consumer one: physics ------------------------------------------------

#[test]
fn physics_moves_the_unfrozen_and_not_the_frozen() {
    let mut w = World::new();
    let moving = w.spawn();
    w.insert(moving, at(0.0, 0.0));
    w.insert(moving, Velocity { dx: 10.0, dy: 0.0 });
    let stuck = w.spawn();
    w.insert(stuck, at(0.0, 0.0));
    w.insert(stuck, Velocity { dx: 10.0, dy: 0.0 });
    w.insert(stuck, Frozen);

    physics::step(&mut w, 0.5);

    assert_eq!(*w.get::<Position>(moving).unwrap(), at(5.0, 0.0));
    assert_eq!(
        *w.get::<Position>(stuck).unwrap(),
        at(0.0, 0.0),
        "a frozen entity keeps its velocity but must not move"
    );
}

// ---- consumer two: render -------------------------------------------------

#[test]
fn render_lists_the_visible_in_layer_order() {
    let mut w = World::new();
    let top = w.spawn();
    w.insert(top, at(0.0, 0.0));
    w.insert(top, Sprite { layer: 9 });
    let bottom = w.spawn();
    w.insert(bottom, at(0.0, 0.0));
    w.insert(bottom, Sprite { layer: 1 });
    let ghost = w.spawn();
    w.insert(ghost, at(0.0, 0.0));
    w.insert(ghost, Sprite { layer: 5 });
    w.insert(ghost, Hidden);

    let list = render::draw_list(&w);
    assert_eq!(
        list.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![bottom, top],
        "hidden is excluded, the rest sorted by layer"
    );
}
