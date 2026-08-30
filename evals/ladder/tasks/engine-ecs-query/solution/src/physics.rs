//! Movement integration: advance every moving entity by its velocity.

use crate::entity::EntityId;
use crate::world::World;
use crate::{Frozen, Position, Velocity};

/// Step every entity that has both a Position and a Velocity.
///
/// Frozen entities keep their Velocity but must not move. The exclusion is the
/// ECS's job now -- this used to collect the frozen set and `contains` it for
/// every candidate, which is O(n*m) and was duplicated in `render`.
pub fn step(world: &mut World, dt: f64) {
    let moves: Vec<(EntityId, f64, f64)> = world
        .iter2_without::<Position, Velocity, Frozen>()
        .map(|(id, _, v)| (id, v.dx * dt, v.dy * dt))
        .collect();
    for (id, dx, dy) in moves {
        if let Some(p) = world.get_mut::<Position>(id) {
            p.x += dx;
            p.y += dy;
        }
    }
}
