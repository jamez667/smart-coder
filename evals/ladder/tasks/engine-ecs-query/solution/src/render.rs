//! Draw-list assembly: gather what should be drawn, in layer order.

use crate::entity::EntityId;
use crate::world::World;
use crate::{Hidden, Position, Sprite};

/// Build the draw list: every entity with a Position and a Sprite, sorted by
/// layer then by entity index for a stable order.
///
/// Hidden entities keep their Sprite but must not draw -- excluded by the same
/// ECS query `physics` uses, rather than by a second copy of the filter.
pub fn draw_list(world: &World) -> Vec<(EntityId, i32)> {
    let mut out: Vec<(EntityId, i32)> = world
        .iter2_without::<Position, Sprite, Hidden>()
        .map(|(id, _, s)| (id, s.layer))
        .collect();
    out.sort_by_key(|(id, layer)| (*layer, id.index));
    out
}
