# 12 — Platform clients (Android app, Windows client) & AICore

## Principle

`dumb-coder` is a **portable Rust core** wrapped in **thin, per-platform shells**.
The core (agent loop, tools, context, eval) knows nothing about Android, Windows,
or any model runtime — it talks only to the `ModelBackend` seam (spec 02) and (in
time) an effects boundary. Each platform supplies those from outside.

Two clients are planned:

| Client | Status | Model backend | Character |
| --- | --- | --- | --- |
| **Android app** | first target | **AICore** (Gemma 4 / Gemini Nano 4) via ML Kit GenAI | on-device, constrained sandbox |
| **Windows client** | later | flexible — Ollama / llama.cpp / OpenAI-compat / remote | capable, full filesystem & tools |

Same core, different shells. This is exactly what the pluggable backend (spec 02)
and the portable-core architecture (spec 01) were for.

```
        ┌──────────────────────┐        ┌──────────────────────────┐
        │   Android app (Kotlin)│        │  Windows client (later)  │
        │  UI + AICore backend  │        │  UI/CLI + flexible backend│
        └───────────┬──────────┘        └────────────┬─────────────┘
            JNI  │   ▲  callback                native │  ▲
                 ▼   │                                 ▼  │
        ┌───────────────────────────────────────────────────────────┐
        │             portable Rust core (dc-core/...)                │
        │   agent loop · tools · context · eval · ModelBackend seam   │
        └───────────────────────────────────────────────────────────┘
```

## The model seam: `CallbackBackend`

The core never calls a runtime directly. `dc_model::CallbackBackend` implements
`ModelBackend` by delegating to an injected closure
`Fn(&GenerateRequest) -> Result<GenerateResponse>`:

- **Android:** the closure performs a **JNI up-call** into the Kotlin AICore
  wrapper and returns the generated text.
- **Windows / desktop / tests:** the closure is a local call (Ollama HTTP,
  llama.cpp, or a mock).

Because the seam is just a closure, the *entire contract is tested on the host*
without any device (see `CallbackBackend` tests in `dc-model`). That's how we make
progress on an on-device target from a Linux/Windows dev box.

## Android app + AICore

### Why AICore
On supported devices AICore runs **Gemma 4 as Gemini Nano 4**, OS-managed: no
weights to ship, hardware-accelerated, big battery wins (spec 10). It is reached
**only from a native app** via the **ML Kit GenAI** APIs (the Prompt API for
custom text generation) — not over any network — which is why the native-app
path is required for AICore.

### Component split
- **Rust core** compiled as a **native library** (`cdylib` → `.so` per ABI) via
  the NDK (e.g. `cargo-ndk`).
- **Kotlin app** owns: the UI, the **AICore backend** (ML Kit GenAI), the Android
  effects (scoped storage, etc.), and the **JNI bridge**.

### The JNI bridge (illustrative — verify signatures on-device)
The core requests a generation; the request crosses JNI up into Kotlin, which
calls AICore and returns the text down to Rust:

```rust
// Rust (cdylib). Illustrative; real impl uses the `jni` crate and careful
// thread/JNIEnv handling. The closure handed to CallbackBackend does the up-call.
let backend = dc_model::CallbackBackend::android_core(|req| {
    // ... attach current thread to the JVM, call the Kotlin callback with the
    // serialized request, receive the response string, map errors ...
    kotlin_generate(req) // -> Result<GenerateResponse>
});
```

```kotlin
// Kotlin side (illustrative — confirm against the current ML Kit GenAI API).
// AICore/ML Kit GenAI is asynchronous; the JNI callback blocks until the result
// is ready and returns a String (run off the main thread).
class AiCoreBackend(context: Context) {
    // 1) check feature availability  2) download if needed  3) generate
    fun generateBlocking(prompt: String): String { /* ML Kit GenAI Prompt API */ }
}
```

> The Kotlin/ML Kit GenAI specifics (artifact id, class names, availability/
> download callbacks, streaming) must be confirmed against Google's current docs
> — they couldn't be fetched while writing this. See the `android/` reference and
> spec 10's sources.

### The harder Android problem: effects, not inference
Inference is the *easy* part (one seam). A coding agent also needs **filesystem
and command execution** (spec 04), and an Android **app sandbox has no shell and
only scoped storage**. So the tool layer needs a platform abstraction:

- Tools become a trait with platform implementations, **or** control is inverted
  so the Kotlin shell provides effects (storage access framework, etc.) to the
  core via callbacks — the same pattern as the model seam.
- Realistic v1 scope on Android: operate over an **app-scoped working directory**
  (e.g. a cloned repo in app storage), no arbitrary shell. Full shell-driven
  coding is where the **Windows client** shines.

This is called out as the main open design item for the Android client.

## Windows client (later)

"More flexible" maps cleanly onto the architecture:

- Same Rust core; a desktop shell (CLI first, per spec 06; GUI optional).
- **Flexible backends:** Ollama / llama.cpp / OpenAI-compat / remote — including
  models up to the 12B ceiling, so the Windows client can be the **T1 architect/
  orchestrator** tier (spec 02) with full tools and filesystem.
- Full effects: real filesystem + shell (spec 04) with the permission layer.

### Optional: clients working together
Because tiers are just profiles (spec 02) and the swarm is hub-and-spoke
(spec 08), a capable **Windows client could act as the orchestrator (T1)** while
an **Android device contributes on-device inference (T2)** — or each runs
standalone. Not required; enabled by the design if wanted later.

## Build & toolchain

- **Android:** `rustup target add aarch64-linux-android` (+ other ABIs) → build
  the `cdylib` with **cargo-ndk** (needs the **Android NDK + SDK**) → drop `.so`s
  into the Gradle module's `jniLibs` → Gradle builds the APK. **None of this runs
  in the project's Linux CI VM** (no NDK/SDK/device); it's an on-device/dev-machine
  step.
- **Windows:** standard `cargo build` for `x86_64-pc-windows-msvc` (or run under
  the existing CLI); flexible backends need no special toolchain.

## What is verifiable where (collaboration reality)

- **Host (Linux/Windows dev):** the portable Rust core — agent loop, tools,
  context, eval, and the `CallbackBackend` seam — is fully unit-testable. This is
  where most logic lives and is proven.
- **On device only:** the Kotlin app, the JNI link, the AICore calls, and the
  end-to-end app. Built and run by the user; the repo provides reference
  scaffolding under `android/`.

The split is deliberate: keep as much as possible in the host-testable Rust core,
so the untestable-here surface (Kotlin/JNI/AICore glue) stays thin.

## Relationship to other specs

- The seam is `CallbackBackend` over `ModelBackend` ([02](02-model-backends.md)).
- Portable core + shells is the architecture of [01](01-architecture.md).
- Effects/tools on Android extend [04](04-tools.md).
- Tiering across clients reuses [02](02-model-backends.md) / [08](08-orchestration-and-swarm.md).
- AICore facts & sources are in [10](10-prior-art.md).
