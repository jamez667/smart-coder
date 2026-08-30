# Vendored from void-claim

`fixture/src/world.rs` and `fixture/src/entity.rs` are `void_engine::ecs` verbatim
(only the `use super::entity` path rewritten for the flatter fixture layout).
`physics.rs` and `render.rs` are NOT from the repo -- they are written for this
task, in the shape of systems that drive that ECS, each hand-rolling the same
"collect the marked set, then contains" workaround the task asks to be replaced.

No external dependencies: the ECS is std-only. The lock file is vendored anyway so
`--offline` never has to resolve.

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
