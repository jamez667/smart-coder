//! A slice of the void-claim engine's ECS, plus two game systems that query it.
//!
//! `entity` and `world` are `void_engine::ecs` verbatim. `physics` and `render`
//! are the kind of systems that drive it: each walks entities holding a
//! particular set of components.

pub mod entity;
pub mod physics;
pub mod render;
pub mod world;

/// Where a thing is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

/// How fast it is going.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Velocity {
    pub dx: f64,
    pub dy: f64,
}

/// What it looks like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sprite {
    pub layer: i32,
}

/// Marks an entity as frozen — it keeps its Velocity but must not move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frozen;

/// Marks an entity as hidden — it keeps its Sprite but must not draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hidden;
