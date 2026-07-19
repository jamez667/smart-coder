package com.smartcoder.remote

/**
 * The JNI boundary between the Kotlin app and the Rust agent core (`libsc_android.so`).
 *
 * Direction of calls:
 *  - Kotlin → Rust: [runTask] hands a task + app-scoped workspace to the agent loop.
 *  - Rust → Kotlin: for each model turn the core calls UP into [onGenerate], which runs
 *    the on-device model (AICore / Gemini Nano) and returns the generated text.
 *
 * The `external fun` signature must match the Rust export
 * `Java_com_smartcoder_remote_NativeBridge_runTask` in the `sc-android` crate, and
 * [onGenerate]'s name/signature must match the JNI up-call constants there
 * (`onGenerate(String, int, float): String`).
 */
class NativeBridge(private val backend: AiCoreBackend) {

    companion object {
        @Volatile private var loaded = false
        /** Load libsc_android.so once. Returns null on success, or an error message. */
        fun tryLoad(): String? = synchronized(this) {
            if (loaded) return null
            return try {
                System.loadLibrary("sc_android")
                loaded = true
                null
            } catch (t: Throwable) {
                "native core failed to load: ${t.message}"
            }
        }
    }

    /**
     * Kotlin → Rust. Runs the agent on `task` within `workspace` (an absolute path the
     * app can write to). Blocks on the calling thread — call it off the main thread.
     * Returns the core's summary string (or an "error: …" sentinel).
     */
    external fun runTask(task: String, workspace: String): String

    /**
     * Rust → Kotlin up-call, invoked by the core for each model turn. Runs the on-device
     * model and returns the generated text. MUST be blocking (the Rust closure returns
     * synchronously) and MUST NOT throw across the JNI boundary — on error it returns a
     * sentinel string the Rust side surfaces as a backend error.
     */
    @Suppress("unused") // called from native code
    fun onGenerate(prompt: String, maxTokens: Int, temperature: Float): String =
        try {
            backend.generateBlocking(prompt, maxTokens, temperature)
        } catch (t: Throwable) {
            "error: on-device generate failed: ${t.message}"
        }
}
