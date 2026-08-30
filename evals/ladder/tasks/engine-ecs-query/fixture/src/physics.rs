//! Movement integration: advance every moving entity by its velocity.

use crate::entity::EntityId;
use crate::world::World;
use crate::{Frozen, Position, Velocity};

/// Step every entity that has both a Position and a Velocity.
///
/// Frozen entities keep their Velocity but must not move, so this collects the
/// frozen set first and skips it — the ECS has no way to express "has A and B
/// but NOT C", so the filter is done by hand, here.
pub fn step(world: &mut World, dt: f64) {
    let frozen: Vec<EntityId> = world.iter::<Frozen>().map(|(id, _)| id).collect();
    let moves: Vec<(EntityId, f64, f64)> = world
        .iter2::<Position, Velocity>()
        .filter(|(id, _, _)| !frozen.contains(id))
        .map(|(id, _, v)| (id, v.dx * dt, v.dy * dt))
        .collect();
    for (id, dx, dy) in moves {
        if let Some(p) = world.get_mut::<Position>(id) {
            p.x += dx;
            p.y += dy;
        }
    }
}
