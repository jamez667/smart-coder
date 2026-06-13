/*
 * AiCoreBackend — on-device text generation via the ML Kit GenAI Prompt API
 * (Gemini Nano / Gemma 4 through AICore), exposed as a blocking generate() that
 * the Rust core calls over JNI.
 *
 * STATUS: REFERENCE — NOT COMPILED OR TESTED HERE (no Android SDK/device in CI).
 * The API surface below matches the ML Kit GenAI Prompt API as of mid-2026:
 *   dependency: com.google.mlkit:genai-prompt:1.0.0-beta2   (beta; may change)
 *   docs: https://developers.google.com/ml-kit/genai/prompt/android/get-started
 *   ref:  https://developers.google.com/android/reference/kotlin/com/google/mlkit/genai/prompt/GenerativeModel
 *   sample: https://github.com/googlesamples/mlkit/tree/master/android/genai
 * VERIFY the download-feature flow and exact package names against the current
 * beta — those are the parts most likely to drift.
 */
package dev.dumbcoder.android

import com.google.mlkit.genai.common.FeatureStatus
import com.google.mlkit.genai.prompt.Generation
import com.google.mlkit.genai.prompt.GenerativeModel
import com.google.mlkit.genai.prompt.generateContentRequest
import kotlinx.coroutines.runBlocking

/**
 * Thin wrapper over the ML Kit GenAI Prompt API.
 *
 * ML Kit GenAI is asynchronous (Kotlin coroutines). The JNI callback that the
 * Rust core invokes is synchronous, so [generateBlocking] bridges with
 * `runBlocking`. Call it OFF the main thread — JNI native calls in our design run
 * on a worker thread, so this is fine.
 */
class AiCoreBackend {

    // Created lazily; reused across turns.
    private val model: GenerativeModel by lazy { Generation.getClient() }

    /** True if the on-device feature is ready (already downloaded). */
    fun isAvailable(): Boolean = runBlocking {
        model.checkStatus() == FeatureStatus.AVAILABLE
    }

    /**
     * Ensure the model is downloaded, then run a single non-streaming generation
     * and return the text.
     *
     * `maxTokens`/`temperature` are accepted from the core for forward
     * compatibility; the current Prompt API beta exposes limited generation
     * config, so they may be applied via the request builder or ignored — wire
     * to GenerationConfig if/when available.
     */
    fun generateBlocking(prompt: String, maxTokens: Int, temperature: Float): String = runBlocking {
        when (model.checkStatus()) {
            FeatureStatus.UNAVAILABLE ->
                throw IllegalStateException("AICore/Gemini Nano not supported on this device")
            FeatureStatus.DOWNLOADABLE -> {
                // TODO: confirm the exact download API/await in the current beta.
                // The get-started guide downloads the feature and reports progress;
                // block here until it completes (or throws).
                model.downloadFeature() // <-- VERIFY: method name / awaiting completion
            }
            FeatureStatus.AVAILABLE -> { /* ready */ }
            else -> { /* treat unknown as ready and let generateContent surface errors */ }
        }

        val request = generateContentRequest { text(prompt) }
        val result = model.generateContent(request) // suspend
        result.text
    }
}
