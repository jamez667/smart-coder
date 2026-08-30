# Vendored from void-claim

`fixture/src/tilegrid.rs` is a verbatim copy of `crates/void_engine/src/tilegrid.rs`
(minus its inline test module), and `fixture/src/tile_collide.rs` is the two
functions it re-exports. `floor.rs` and `module.rs` are modelled on
`void_sim::station_interior::floor` and `void_sim::module` -- the two real,
independent consumers of `TileGrid`. `Floor::tile_regions` and `Floor::find_first`
are the originals, verbatim, including the hand-rolled flood-fill this task asks to
be lifted into the engine.

Unlike the pathfind tasks this one is a REAL cargo crate with the engine's real
`glam` dependency, because `TileGrid` uses `DVec2` and stripping it would change
the code under test. That is affordable: glam is already in the local cargo cache,
so a cold build of this fixture is ~4 seconds.

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
