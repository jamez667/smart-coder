# Android client (reference scaffolding)

> **Status: reference only — not built or tested here.** The project's CI runs on
> a Linux VM with **no Android SDK/NDK and no device**, so this directory holds
> *illustrative* Kotlin/JNI reference for the Android app described in
> [`../docs/specs/12-platform-clients.md`](../docs/specs/12-platform-clients.md).
> Build and run it on a development machine + device. Treat the ML Kit GenAI calls
> as **TODO: verify against the current API** — exact class/method names could not
> be fetched while writing this.

## What the Android client is

A native app that runs the portable Rust core as a `cdylib` and supplies
inference from **AICore** (Gemma 4 / Gemini Nano 4) via the **ML Kit GenAI**
APIs. The Rust core stays platform-agnostic; the only thing crossing the boundary
is the model seam (`dc_model::CallbackBackend`) and, later, an effects boundary
for filesystem access.

```
Kotlin UI ─▶ Rust core (.so) ──(needs a generation)──▶ JNI callback ─▶ AiCoreBackend ─▶ AICore
                  ▲                                                                        │
                  └──────────────────────── generated text ◀──────────────────────────────┘
```

## Build outline (on a machine with the Android SDK + NDK)

1. Add Rust Android targets: `rustup target add aarch64-linux-android` (+ other ABIs).
2. Install [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk): `cargo install cargo-ndk`.
3. Build the core as a shared lib per ABI and place the `.so`s into the Gradle
   module's `src/main/jniLibs/<abi>/` (a `dc-android` cdylib crate exposing the
   JNI entry points is future work — see spec 12).
4. Add the ML Kit GenAI dependency to the app `build.gradle` (confirm the current
   artifact id from Google's docs).
5. Build the APK with Gradle and run on a device that supports AICore (flagship
   SoC, sufficient RAM — see spec 10).

## Files here

- `kotlin/AiCoreBackend.kt` — wraps ML Kit GenAI / AICore as a blocking
  `generate(prompt)`; the JNI callback calls this.
- `kotlin/NativeBridge.kt` — `external fun` declarations for the Rust core and the
  callback the core invokes for inference.

Both are **illustrative**; wire them up against the real Rust JNI exports and the
current ML Kit GenAI API.
