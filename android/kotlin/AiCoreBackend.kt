/*
 * AiCoreBackend — wraps on-device Gemini Nano (Gemma 4) via the ML Kit GenAI
 * APIs, exposed as a blocking generate() that the Rust core calls over JNI.
 *
 * STATUS: ILLUSTRATIVE REFERENCE — NOT COMPILED OR TESTED HERE.
 * The exact ML Kit GenAI class/method names, the Gradle artifact, and the
 * availability/download flow MUST be confirmed against Google's current docs:
 *   https://developers.google.com/ml-kit/genai/prompt/android
 * The structure (check availability -> ensure downloaded -> generate) is stable;
 * the precise API surface is what to verify.
 */
package dev.dumbcoder.android

import android.content.Context

/**
 * Thin wrapper over ML Kit GenAI / AICore.
 *
 * ML Kit GenAI is asynchronous. The JNI callback that the Rust core invokes is
 * expected to block until a result is ready, so [generateBlocking] bridges the
 * async API to a synchronous return. Call it OFF the main thread (JNI native
 * calls already run on a worker thread in our design).
 */
class AiCoreBackend(private val context: Context) {

    /**
     * Run a single non-streaming generation and return the text.
     *
     * Pseudocode against ML Kit GenAI (verify names):
     *   1. val model = Generation.getClient(options)            // or GenerativeModel(...)
     *   2. check feature status; if downloadable, trigger download and await
     *   3. val result = model.generateContent(prompt).await()   // Tasks/coroutine
     *   4. return result.text
     *
     * On failure (feature unavailable, download pending, device unsupported)
     * throw; the Rust side maps the exception to a DcError::Backend so the agent
     * loop can react (e.g. surface to the user) rather than crashing.
     */
    fun generateBlocking(prompt: String, maxTokens: Int, temperature: Float): String {
        // TODO: implement against the current ML Kit GenAI Prompt API.
        throw NotImplementedError("wire to ML Kit GenAI Prompt API (AICore)")
    }

    /** True if AICore/Gemini Nano is supported and ready on this device. */
    fun isAvailable(): Boolean {
        // TODO: ML Kit GenAI feature-status check.
        return false
    }
}
