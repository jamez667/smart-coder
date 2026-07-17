# 12 — Platform clients (Windows)

## Principle

`smart-coder` is a **portable Rust core** wrapped in **thin, per-platform shells**.
The core (agent loop, tools, context, eval) knows nothing about any particular OS
or model runtime — it talks only to the `ModelBackend` seam (spec 02) and (in
time) an effects boundary. Each platform supplies those from outside.

The shipped client is the **Windows desktop** shell (CLI first per spec 06, plus
the `sc-win` GUI). Same core, thin shell. This is exactly what the pluggable
backend (spec 02) and the portable-core architecture (spec 01) were for.

```
        ┌──────────────────────────┐
        │   Windows client          │
        │  UI/CLI + flexible backend│
        └────────────┬─────────────┘
                native │  ▲
                       ▼  │
        ┌───────────────────────────────────────────────────────────┐
        │             portable Rust core (sc-core/...)                │
        │   agent loop · tools · context · eval · ModelBackend seam   │
        └───────────────────────────────────────────────────────────┘
```

## The model seam

The core never calls a runtime directly; it goes through `ModelBackend`
(spec 02). On the desktop that's `OpenAiBackend` against Ollama / llama.cpp /
vLLM / any OpenAI-compatible server, or a `MockBackend` for tests. Because the
seam is a trait, the *entire contract is tested on the host* without a live
model — that's how most of the logic is proven.

## Windows client

"Flexible" maps cleanly onto the architecture:

- Same Rust core; a desktop shell (CLI first, per spec 06; `sc-win` GUI).
- **Flexible backends:** Ollama / llama.cpp / OpenAI-compat / remote — including
  models up to the 12B ceiling, so the Windows client can be the **T1 architect/
  orchestrator** tier (spec 02) with full tools and filesystem.
- Full effects: real filesystem + shell (spec 04) with the permission layer.

## Build & toolchain

- **Windows:** standard `cargo build` for `x86_64-pc-windows-msvc` (or run under
  the existing CLI); flexible backends need no special toolchain.

## Relationship to other specs

- The seam is `ModelBackend` ([02](02-model-backends.md)).
- Portable core + shells is the architecture of [01](01-architecture.md).
- Effects/tools are [04](04-tools.md).
- Tiering reuses [02](02-model-backends.md) / [08](08-orchestration-and-swarm.md).
