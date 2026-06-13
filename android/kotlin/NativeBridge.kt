/*
 * NativeBridge — the JNI boundary between the Kotlin app and the portable Rust
 * core (built as a cdylib / .so).
 *
 * STATUS: ILLUSTRATIVE REFERENCE — NOT COMPILED OR TESTED HERE.
 * The `external fun` signatures must match the Rust JNI exports (a future
 * `dc-android` cdylib crate). See ../docs/specs/12-platform-clients.md.
 *
 * Direction of calls:
 *   - Kotlin -> Rust : start a task / drive the agent core (downward).
 *   - Rust  -> Kotlin: when the core needs a generation, it calls back UP into
 *                      [onGenerate], which delegates to AiCoreBackend (AICore).
 */
package dev.dumbcoder.android

class NativeBridge(private val backend: AiCoreBackend) {

    companion object {
        init {
            // Loads libdc_android.so (the Rust core packaged in jniLibs/).
            System.loadLibrary("dc_android")
        }
    }

    /**
     * Kotlin -> Rust. Hands a task to the agent core. The core will call
     * [onGenerate] (below) whenever it needs the model. Returns a result/summary
     * string. Signature is illustrative; align with the Rust export.
     */
    external fun runTask(task: String): String

    /**
     * Rust -> Kotlin callback. Invoked by the core for each model turn. Runs the
     * on-device model via AICore and returns the generated text. Kept blocking so
     * the Rust `CallbackBackend` closure can return synchronously.
     *
     * Must NOT throw across the JNI boundary uncaught; on error, return a
     * sentinel the Rust side maps to DcError::Backend (or use a status+payload
     * convention agreed with the Rust export).
     */
    fun onGenerate(prompt: String, maxTokens: Int, temperature: Float): String {
        return backend.generateBlocking(prompt, maxTokens, temperature)
    }
}
