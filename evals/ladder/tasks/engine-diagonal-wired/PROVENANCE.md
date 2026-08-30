# Vendored from void-claim

`fixture/pathfind.rs` is a verbatim copy of `crates/void_engine/src/pathfind.rs`
from the void-claim game engine, taken 2026-08-29, minus its inline `#[cfg(test)]`
module (this task ships its own contract test in `fixture/test.rs`).

`fixture/nav.rs` is modelled on `crates/void_sim/src/pathfind.rs` -- the game-side
wrapper that owns the `TileSource` impls. It is NOT verbatim: the real module
imports `Floor` and `TileKind` from `station_interior`, which drags in the rest of
the sim crate. Those two types are inlined here in their real shape so the pair
compiles standalone. The wrapper functions, their signatures and their docs are
the originals.

**Why vendored rather than pointed at the live repo:** an eval fixture must be
frozen. If the task read the engine's working tree, a change there would silently
alter what the model is scored on, and a result from last month would stop being
comparable to one from today.

**Why this module:** it uses only `std`, so it verifies with bare `rustc` in
seconds. The crate it comes from pulls `wgpu` and `winit` — 32s warm and minutes
cold — and the harness copies each fixture into a fresh workspace with no
`target/` dir, so a real cargo build would dominate every verify, and the model
verifies repeatedly.

Re-vendor by copying the file again and stripping the test module. If the upstream
API changes shape, the contract test in `fixture/test.rs` is what needs revisiting.
