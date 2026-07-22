# smart-coder — Android app

The phone client for `smart-coder`. It runs in two modes:

- **Remote mirror** — attach to a live `sc-win` desktop session over Tailscale
  (chat, activity feed, approve/send-back, project switch). This mirrors the
  desktop's running session; see the phone-mirror pieces `sc-web` + `sc-iterate`
  in the workspace.
- **On-device** — run the whole agent loop *locally* on the phone, with inference
  by Android **AICore (Gemini Nano)**. The Rust agent core is called through a
  JNI bridge in the [`sc-android`](../crates/sc-android) crate; each model turn
  calls up into Kotlin's `onGenerate(...)`, which runs AICore.

App id `com.smartcoder.remote`, `minSdk = 26`. Kotlin sources are under
[`app/src/main/kotlin/com/smartcoder/remote/`](app/src/main/kotlin/com/smartcoder/remote/):
`MainActivity`, `ScClient` (remote-mirror client), `NativeBridge` (JNI into
`sc-android`), `AiCoreBackend` (on-device inference).

## Building

Prerequisites: JDK 17+, the Android SDK, and — for on-device mode — the Rust
Android targets and the NDK to build the `sc-android` native library.

```sh
# from this directory
./gradlew assembleDebug        # Linux / macOS
.\gradlew.bat assembleDebug    # Windows
```

The native `.so` for on-device mode is built from `crates/sc-android` and placed
under `app/src/main/jniLibs/`. Those `.so` outputs are gitignored and
regenerable — see the repo-root [`.gitignore`](../.gitignore). Remote-mirror
mode does not require the native library.

> Cleartext HTTP to the loopback/Tailscale mirror is permitted by
> [`network_security_config.xml`](app/src/main/res/xml/network_security_config.xml)
> — this is intentional for the local/Tailscale-only mirror connection.
