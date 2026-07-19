package com.smartcoder.remote

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.BufferedReader
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder

/**
 * Thin HTTP client for the smart-coder remote iterate server (the Rust `serve_iterate`).
 *
 * Matches the server contract in `crates/sc-web/src/iterate_server.rs`:
 *  - GET  routes carry the token as `?k=<token>` (the URL the phone was handed).
 *  - POST routes carry `Authorization: Bearer <token>` (the CSRF defense).
 *
 * All calls are blocking `HttpURLConnection` under the hood, dispatched on IO. The
 * event feed is a **short poll** of `/events?from=N` — the server is single-threaded,
 * so we must NOT hold a long-poll open (it would starve the POST routes).
 */
class ScClient(
    /** e.g. "http://100.x.y.z:8178" (a Tailscale IP) — no trailing slash, no query. */
    baseUrl: String,
    private val token: String,
) {
    private val base = baseUrl.trimEnd('/')

    /** One decoded event: its `type` tag plus the raw object for field access. */
    data class Event(val type: String, val obj: JSONObject)

    /** A batch from `/events`: the events, the next cursor, and whether the run ended. */
    data class Events(val events: List<Event>, val next: Int, val done: Boolean)

    data class Status(val workspace: String, val running: Boolean, val done: Boolean, val events: Int)

    suspend fun status(): Status = withContext(Dispatchers.IO) {
        val body = get("/status?k=${enc(token)}")
        val o = JSONObject(body)
        Status(
            workspace = o.optString("workspace", "(workspace)"),
            running = o.optBoolean("running", false),
            done = o.optBoolean("done", false),
            events = o.optInt("events", 0),
        )
    }

    suspend fun events(from: Int): Events = withContext(Dispatchers.IO) {
        val body = get("/events?from=$from&k=${enc(token)}")
        val o = JSONObject(body)
        val arr = o.getJSONArray("events")
        val list = ArrayList<Event>(arr.length())
        for (i in 0 until arr.length()) {
            val e = arr.getJSONObject(i)
            list.add(Event(e.optString("type", "?"), e))
        }
        Events(list, o.optInt("next", from), o.optBoolean("done", false))
    }

    /** Start an iterate run. Returns the HTTP status + any error body so the UI can show
     *  exactly why it failed (401 bad token, 409 already active, timeout, etc.). */
    suspend fun run(task: String): Result = withContext(Dispatchers.IO) {
        val body = JSONObject().put("kind", "iterate").put("task", task).toString()
        postResult("/run", body)
    }

    /** Send a chat message to the live desktop session (the mirror's /chat route). */
    suspend fun chat(text: String): Result = withContext(Dispatchers.IO) {
        postResult("/chat", JSONObject().put("text", text).toString())
    }

    data class Project(val name: String, val path: String)
    data class ProjectList(val current: String?, val projects: List<Project>)

    /** List the desktop's recent projects + which one is currently open. */
    suspend fun projects(): ProjectList = withContext(Dispatchers.IO) {
        val o = JSONObject(get("/projects?k=${enc(token)}"))
        val arr = o.optJSONArray("projects")
        val list = ArrayList<Project>(arr?.length() ?: 0)
        if (arr != null) for (i in 0 until arr.length()) {
            val p = arr.getJSONObject(i)
            list.add(Project(p.optString("name"), p.optString("path")))
        }
        ProjectList(o.optString("current").ifEmpty { null }, list)
    }

    /** Ask the desktop to switch to a recent project (by its path). */
    suspend fun open(path: String): Result = withContext(Dispatchers.IO) {
        postResult("/open", JSONObject().put("path", path).toString())
    }

    /** An HTTP outcome: the status code (or 0 on a transport error) and a short detail. */
    data class Result(val code: Int, val detail: String) {
        val ok get() = code == 200
    }

    suspend fun approve(id: Long): Boolean = withContext(Dispatchers.IO) {
        post("/approve", JSONObject().put("id", id).toString()) == 200
    }

    suspend fun deny(id: Long, reason: String = "denied"): Boolean = withContext(Dispatchers.IO) {
        post("/deny", JSONObject().put("id", id).put("reason", reason).toString()) == 200
    }

    suspend fun cancel(): Boolean = withContext(Dispatchers.IO) {
        post("/cancel", "{}") == 200
    }

    // ---- transport ----------------------------------------------------------

    private fun get(path: String): String {
        val conn = (URL(base + path).openConnection() as HttpURLConnection).apply {
            requestMethod = "GET"
            connectTimeout = 5000
            readTimeout = 8000
        }
        return conn.use { readBody(it) }
    }

    /** POST returning the status code AND any error-body/exception text, for diagnostics. */
    private fun postResult(path: String, json: String): Result {
        return try {
            val conn = (URL(base + path).openConnection() as HttpURLConnection).apply {
                requestMethod = "POST"
                connectTimeout = 5000
                readTimeout = 8000
                doOutput = true
                setRequestProperty("Authorization", "Bearer $token")
                setRequestProperty("Content-Type", "application/json")
            }
            conn.use {
                it.outputStream.use { os -> os.write(json.toByteArray()) }
                val code = it.responseCode
                val body = readBody(it).take(120)
                Result(code, if (code == 200) "ok" else "HTTP $code $body")
            }
        } catch (t: Throwable) {
            Result(0, t.message ?: t.javaClass.simpleName)
        }
    }

    /** POST JSON with the bearer header; returns the HTTP status code. */
    private fun post(path: String, json: String): Int {
        val conn = (URL(base + path).openConnection() as HttpURLConnection).apply {
            requestMethod = "POST"
            connectTimeout = 5000
            readTimeout = 8000
            doOutput = true
            setRequestProperty("Authorization", "Bearer $token")
            setRequestProperty("Content-Type", "application/json")
        }
        return conn.use {
            it.outputStream.use { os -> os.write(json.toByteArray()) }
            // Drain the body so the connection can be reused; ignore content.
            readBody(it)
            it.responseCode
        }
    }

    private fun readBody(conn: HttpURLConnection): String {
        val stream = if (conn.responseCode in 200..299) conn.inputStream else conn.errorStream
        return stream?.bufferedReader()?.use(BufferedReader::readText) ?: ""
    }

    private inline fun <T> HttpURLConnection.use(block: (HttpURLConnection) -> T): T =
        try {
            block(this)
        } finally {
            disconnect()
        }

    private fun enc(s: String): String = URLEncoder.encode(s, "UTF-8")

    companion object {
        /**
         * Parse a pasted dashboard URL like `http://host:8178/?k=<token>` into a
         * (baseUrl, token) pair. Returns null if it has no `k=` token.
         */
        fun fromUrl(raw: String): Pair<String, String>? {
            val u = raw.trim()
            val q = u.substringAfter('?', "")
            val token = q.split('&')
                .firstOrNull { it.startsWith("k=") }
                ?.substringAfter("k=")
                ?.let { java.net.URLDecoder.decode(it, "UTF-8") }
                ?: return null
            val base = u.substringBefore('?').trimEnd('/')
            return base to token
        }
    }
}
