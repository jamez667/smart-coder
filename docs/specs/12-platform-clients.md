# 12 — Platform clients (Windows, Android)

## Principle

`smart-coder` is a **portable Rust core** wrapped in **thin, per-platform shells**.
The core (agent loop, tools, context, eval) knows nothing about any particular OS
or model runtime — it talks only to the `ModelBackend` seam (spec 02) and (in
time) an effects boundary. Each platform supplies those from outside.

Two shells ship: the **Windows desktop** (CLI first per spec 06, plus the
`sc-win` GUI) and the **Android phone client**. Same core, thin shells. This is
exactly what the pluggable backend (spec 02) and the portable-core architecture
(spec 01) were for — the phone is the case that proves it, since it reaches the
core through a JNI boundary and supplies inference from Kotlin.

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
- The GUI's checkpoint surface is PR-style review rather than a prompt: the
  reviewer opens any phase artifact in the code view and leaves line comments, and
  **Send back** turns them into the feedback *and* the target phase
  ([09](09-workflow-and-checkpoints.md)). It surfaces no separate *revise* button —
  editing by comment supersedes it, though `Decision::Revise` remains in the engine
  for the CLI.

## Android client

The phone client (`android/`, app id `com.smartcoder.remote`, `minSdk = 26`) runs
in **two modes**, and they share nothing but the app shell:

- **Remote mirror.** Attaches to a *live* `sc-win` desktop session over Tailscale
  — chat, activity feed, approve/send-back, project switch. The phone is a view
  onto a run happening on the desktop; no agent runs here. Served by `sc-web`
  from inside the desktop process, gated by a per-run token
  ([20](20-remote-review.md) covers the review surface this feeds).
- **On-device.** The whole agent loop runs *on the phone*, with inference by
  Android **AICore (Gemini Nano)**. The core is called through the JNI bridge in
  <!--@ crates/sc-android/src/lib.rs --> `sc-android`; each model turn up-calls
  into Kotlin's `onGenerate(...)`, which runs AICore and blocks for the answer.

**The up-call is the whole design.** `ModelBackend` (spec 02) is a synchronous
trait, and AICore is an async Kotlin API — so the Rust side blocks on a channel
that the Kotlin side fills. That inversion is what lets an on-device model
satisfy the same seam as an HTTP endpoint, and it is why the ≤12B thesis in
[00](00-overview.md) has a phone-sized proof rather than only a desktop one.

**What is and is not verifiable here.** The JNI crate compiles on the host
against the real `jni` API and its pure helpers are unit-tested, but **on-device
behaviour can only be verified on a device with AICore** — there is no such
runtime on a dev box, and CI cannot stand one up. So the host build proves the
boundary type-checks and nothing more. That limit is stated rather than papered
over: the remote-mirror mode is the one exercised daily, and the on-device mode
is the experimental half.

## Build & toolchain

- **Windows:** standard `cargo build` for `x86_64-pc-windows-msvc` (or run under
  the existing CLI); flexible backends need no special toolchain.
- **Android:** Gradle builds the app; `sc-android` is built as a `cdylib`
  (`libsc_android.so`) for the device ABI and loaded by `NativeBridge.tryLoad()`.
  The crate stays a workspace member so `cargo check`/`clippy`/`test` cover it on
  every gate run — the boundary compiling is the one thing CI *can* prove, and it
  is worth the seconds it costs.

## Relationship to other specs

- The seam is `ModelBackend` ([02](02-model-backends.md)).
- Portable core + shells is the architecture of [01](01-architecture.md).
- Effects/tools are [04](04-tools.md).
- Tiering reuses [02](02-model-backends.md) / [08](08-orchestration-and-swarm.md).
- A further `Gate` implementation runs on the remote surface
  ([20](20-remote-review.md)), submitting the same decisions from a phone — and
  reaching this client's conclusion on *revise* independently, offering send-back
  with a note instead. The distinction that matters is the trust boundary: a
  platform client runs *beside the workspace* with full effects and the permission
  layer; the remote surface runs *away from it* with none
  ([18](18-task-intake.md)).
