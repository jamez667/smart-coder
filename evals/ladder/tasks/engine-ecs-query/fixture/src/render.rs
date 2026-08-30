//! Draw-list assembly: gather what should be drawn, in layer order.

use crate::entity::EntityId;
use crate::world::World;
use crate::{Hidden, Position, Sprite};

/// Build the draw list: every entity with a Position and a Sprite, sorted by
/// layer then by entity index for a stable order.
///
/// Hidden entities keep their Sprite but must not draw, so — as in `physics` —
/// the exclusion is collected and filtered by hand.
pub fn draw_list(world: &World) -> Vec<(EntityId, i32)> {
    let hidden: Vec<EntityId> = world.iter::<Hidden>().map(|(id, _)| id).collect();
    let mut out: Vec<(EntityId, i32)> = world
        .iter2::<Position, Sprite>()
        .filter(|(id, _, _)| !hidden.contains(id))
        .map(|(id, _, s)| (id, s.layer))
        .collect();
    out.sort_by_key(|(id, layer)| (*layer, id.index));
    out
}
