//! A slice of the void-claim engine and the two game-side types that wrap it.
//!
//! `tilegrid` is `void_engine::tilegrid` verbatim. `module` and `floor` are the
//! two independent consumers, each owning a `TileGrid<TileKind>` and exposing
//! its own `tile_at`.

pub mod floor;
pub mod tile_collide;
pub mod module;
pub mod tilegrid;

/// What a tile is made of. Shared by both consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TileKind {
    /// Off-grid / nothing here.
    #[default]
    Empty,
    /// Walkable station floor.
    Floor,
    /// Solid.
    Wall,
    /// A docking pad.
    Pad,
    /// Where a player spawns.
    PlayerSpawn,
    /// An elevator car tile; contiguous runs form one car.
    Elevator,
}
