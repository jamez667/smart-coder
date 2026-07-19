package com.smartcoder.remote

import android.content.Context
import com.google.mlkit.genai.common.DownloadStatus
import com.google.mlkit.genai.common.FeatureStatus
import com.google.mlkit.genai.prompt.GenerateContentRequest
import com.google.mlkit.genai.prompt.Generation
import com.google.mlkit.genai.prompt.GenerativeModel
import com.google.mlkit.genai.prompt.PromptPrefix
import com.google.mlkit.genai.prompt.TextPart
import kotlinx.coroutines.runBlocking

/**
 * The on-device model backend, wrapping ML Kit GenAI (Gemini Nano via AICore).
 *
 * The ML Kit GenAI Prompt API is coroutine-based (`suspend` funcs). Since the JNI
 * up-call ([NativeBridge.onGenerate]) needs a *blocking* call, we bridge with
 * `runBlocking` on the (already background) worker thread.
 *
 * NOTE: this API is beta and device-gated (Pixel 8+/9, some Galaxy S24+). It compiles
 * here but only runs on a supported device; every path degrades to a clear message
 * rather than crashing.
 */
class AiCoreBackend(@Suppress("unused") private val context: Context) {

    private val model: GenerativeModel by lazy { Generation.getClient() }

    /** A human-readable status line: is on-device inference usable on this device? */
    fun status(): String = try {
        when (runBlocking { model.checkStatus() }) {
            FeatureStatus.AVAILABLE -> "on-device model: ready (Gemini Nano)"
            FeatureStatus.DOWNLOADABLE -> "on-device model: available — will download on first run"
            FeatureStatus.DOWNLOADING -> "on-device model: downloading…"
            FeatureStatus.UNAVAILABLE -> "on-device model: not supported on this device"
            else -> "on-device model: unknown status"
        }
    } catch (t: Throwable) {
        "on-device model: unavailable (${t.message})"
    }

    /** Ensure the model is present, downloading it if needed. Blocks until settled. */
    fun ensureDownloaded(): Boolean = try {
        when (runBlocking { model.checkStatus() }) {
            FeatureStatus.AVAILABLE -> true
            FeatureStatus.DOWNLOADABLE, FeatureStatus.DOWNLOADING -> {
                runBlocking { model.download().collect { _: DownloadStatus -> } }
                runBlocking { model.checkStatus() } == FeatureStatus.AVAILABLE
            }
            else -> false
        }
    } catch (t: Throwable) {
        false
    }

    /**
     * Generate synchronously — the JNI up-call blocks on this. Builds a full
     * [GenerateContentRequest] so Gemini Nano is driven with the harness's settings:
     *  - the Rust core's `temperature` and `maxTokens`,
     *  - a low `topK` for deterministic, on-format output (a tiny model wanders otherwise),
     *  - a [PromptPrefix] that primes Nano as a tool-calling coding agent — the flattened
     *    transcript already carries the tool contract, but the prefix keeps Nano from
     *    chatting instead of emitting the next tool-call JSON.
     * Returns the model's text, or throws (mapped to an error sentinel by the caller).
     */
    fun generateBlocking(prompt: String, maxTokens: Int, temp: Float): String =
        runBlocking {
            val builder = GenerateContentRequest.Companion.builder(TextPart(prompt))
            builder.temperature = temp
            builder.topK = 1
            // This AICore/Nano build caps output at 256 tokens (larger errors out).
            builder.maxOutputTokens = maxTokens.coerceIn(1, 256)
            builder.candidateCount = 1
            builder.promptPrefix = PromptPrefix(SYSTEM_PREFIX)
            val response = model.generateContent(builder.build())
            response.candidates.firstOrNull()?.text.orEmpty()
        }

    /** Plain Q&A: send a message, get a conversational reply. No agent/tool prompting —
     *  this is the chat mode, so Nano answers naturally instead of emitting tool JSON. */
    fun chat(message: String): String =
        runBlocking {
            val builder = GenerateContentRequest.Companion.builder(TextPart(message))
            builder.temperature = 0.7f
            builder.topK = 40
            builder.maxOutputTokens = 256
            builder.candidateCount = 1
            val response = model.generateContent(builder.build())
            response.candidates.firstOrNull()?.text.orEmpty()
        }

    private companion object {
        // Primes Nano to act like the harness expects: emit exactly one tool call as JSON,
        // no prose. The flattened transcript supplies the full tool list + task each turn.
        const val SYSTEM_PREFIX =
            "You are a coding agent. Each turn, respond with EXACTLY ONE tool call as a single " +
                "JSON object like {\"tool\":\"read_file\",\"path\":\"...\"} and NOTHING else — no " +
                "explanation, no markdown. Use the tools and file list given in the conversation."
    }
}
