# Android client (reference scaffolding)

> **Status: reference scaffolding — not built or tested here.** The project's CI
> runs on a Linux VM with **no Android SDK/NDK and no device**, so this is a
> complete-but-unbuilt module skeleton for the Android app described in
> [`../docs/specs/12-platform-clients.md`](../docs/specs/12-platform-clients.md).
> The Kotlin uses the real ML Kit GenAI Prompt API; build and run it on a
> development machine + device, verifying the download-feature flow against the
> current beta.

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

## Module layout

```
android/
├── settings.gradle.kts        # includes :app
├── build.gradle.kts           # root (AGP + Kotlin plugin versions)
├── gradle.properties
└── app/
    ├── build.gradle.kts       # genai-prompt + coroutines deps; minSdk 26; arm64-v8a
    └── src/main/
        ├── AndroidManifest.xml
        ├── jniLibs/<abi>/      # libdc_android.so dropped here (built by cargo-ndk)
        └── kotlin/dev/dumbcoder/android/
            ├── MainActivity.kt   # button -> runTask -> shows result
            ├── NativeBridge.kt   # JNI: runTask (down) + onGenerate (up)
            └── AiCoreBackend.kt  # ML Kit GenAI Prompt API wrapper
```

## Build outline (on a machine with the Android SDK + NDK)

1. Add Rust Android targets: `rustup target add aarch64-linux-android` (+ other ABIs).
2. Install [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk): `cargo install cargo-ndk`.
3. From the repo root, build the **`dc-android`** cdylib per ABI straight into the
   module's jniLibs:
   `cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release -p dc-android`.
4. Open `android/` in Android Studio (or `./gradlew :app:assembleDebug`) — the
   `genai-prompt` dependency and `minSdk 26` are already set in `app/build.gradle.kts`.
5. Run on a device that supports AICore (flagship SoC, sufficient RAM — see spec 10).

## Status of each file

| File | State |
| --- | --- |
| `NativeBridge.kt` | signatures match the `dc-android` Rust exports |
| `AiCoreBackend.kt` | real ML Kit GenAI Prompt API calls; **verify the download-feature flow** against the current beta |
| `MainActivity.kt`, Gradle files, manifest | minimal skeleton; **untested** — version-bump and adjust as needed |

Nothing here has been compiled or run (no Android SDK/NDK/device in CI). The Rust
half (`crates/dc-android`) *is* compiled against the real `jni` API and tested.
