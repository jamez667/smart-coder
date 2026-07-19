package com.smartcoder.remote

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject

/** Run a blocking block on the IO dispatcher (for JNI/AICore calls that block a thread). */
private suspend fun <T> withContextIO(block: () -> T): T = withContext(Dispatchers.IO) { block() }

// --- Design tokens: mirror the Windows desktop app (Tokyo Night) so the two clients
//     read as one product — dark canvas, flat panels, an orange Send call-to-action.
private object Sc {
    val Bg = Color(0xFF16161E)          // window background
    val Surface = Color(0xFF1B1D2A)     // panel / card
    val InputBg = Color(0xFF262A3D)     // composer fill
    val Border = Color(0xFF33374F)      // hairline card border
    val Fg = Color(0xFFD6DCEC)          // primary text
    val FgMuted = Color(0xFF858CA8)     // labels / hints
    val Accent = Color(0xFFF58C3D)      // links / active — orange, matching the action color
    val Good = Color(0xFF73C78C)
    val Bad = Color(0xFFED7380)
    val Orange = Color(0xFFF58C3D)      // primary action (Send) — the desktop signature
    // Flat, near-square corners to match the desktop (whose RADIUS = 0). A 2dp softening
    // keeps edges from looking accidental without going "rounded".
    val Shape = RoundedCornerShape(2.dp)
}

private val ScColors = darkColorScheme(
    primary = Sc.Orange,
    onPrimary = Color(0xFF201007),
    background = Sc.Bg,
    onBackground = Sc.Fg,
    surface = Sc.Surface,
    onSurface = Sc.Fg,
    surfaceVariant = Sc.InputBg,
    onSurfaceVariant = Sc.FgMuted,
    outline = Sc.Border,
)

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            MaterialTheme(colorScheme = ScColors) {
                Surface(color = Sc.Bg) { ChatScreen() }
            }
        }
    }
}

private enum class Mode { REMOTE, ON_DEVICE, CHAT }

/** A pending approval the user must answer, lifted from the event stream. */
private data class Pending(val id: Long, val command: String, val reason: String)

/** One rendered log line + a kind so we can color it. STREAM is the transient live-typing
 *  line (updated per ChatDelta token, removed when the final ChatMessage lands). */
private data class LogLine(val text: String, val kind: Kind) {
    enum class Kind { MODEL, CALL, OK, ERR, META, STREAM }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ChatScreen() {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val scope = (ctx as ComponentActivity).lifecycleScope

    var urlText by rememberSaveable { mutableStateOf("") }
    var client by remember { mutableStateOf<ScClient?>(null) }
    var connected by rememberSaveable { mutableStateOf(false) }
    var mode by rememberSaveable { mutableStateOf(Mode.REMOTE) }
    var task by rememberSaveable { mutableStateOf("") }
    var running by remember { mutableStateOf(false) }
    var statusLine by remember { mutableStateOf("not connected") }
    val log = remember { mutableStateListOf<LogLine>() }
    val pending = remember { mutableStateListOf<Pending>() }
    // The desktop's current repo + recent-projects list (Remote mode).
    var currentProject by remember { mutableStateOf<String?>(null) }
    val projects = remember { mutableStateListOf<ScClient.Project>() }
    var showPicker by remember { mutableStateOf(false) }

    // Keep the project list + current repo fresh while connected (cheap poll).
    LaunchedEffect(client) {
        val c = client ?: return@LaunchedEffect
        while (isActive) {
            runCatching { c.projects() }.onSuccess {
                currentProject = it.current
                projects.clear(); projects.addAll(it.projects)
            }
            delay(3000)
        }
    }

    // On-device: AICore backend + JNI bridge, created lazily so the app still runs on
    // devices without AICore.
    val aiCore = remember { AiCoreBackend(ctx.applicationContext) }
    val bridge = remember { NativeBridge(aiCore) }
    var onDeviceStatus by remember { mutableStateOf("") }
    LaunchedEffect(mode) {
        onDeviceStatus = if (mode == Mode.ON_DEVICE || mode == Mode.CHAT) {
            withContextIO { aiCore.status() }
        } else ""
    }

    // Poll loop: while connected in Remote mode, drain /events into the log + pending list.
    LaunchedEffect(client) {
        val c = client ?: return@LaunchedEffect
        var from = 0
        val resolved = HashSet<Long>()
        while (isActive) {
            try {
                val batch = c.events(from)
                for (e in batch.events) foldEvent(e, log, pending, resolved)
                from = batch.next
                running = !batch.done && from > 0
                statusLine = when {
                    batch.done && from > 0 -> "run finished"
                    from > 0 -> "running…"
                    else -> "connected — idle"
                }
            } catch (t: Throwable) {
                statusLine = "poll error: ${t.message}"
            }
            delay(700)
        }
    }

    fun startRemote() {
        val c = client ?: return
        val t = task.trim(); if (t.isEmpty()) return
        task = ""
        // Remote = chat with the LIVE desktop session (the mirror). The desktop echoes
        // our own message back as a ChatMessage(you) event, so don't add it locally —
        // just report a send failure if the POST doesn't land.
        scope.launch {
            val r = c.chat(t)
            if (!r.ok) log.add(LogLine("send failed: ${r.detail}", LogLine.Kind.ERR))
        }
    }

    fun startOnDevice() {
        val t = task.trim(); if (t.isEmpty()) return
        task = ""
        running = true
        log.add(LogLine("▸ $t", LogLine.Kind.META))
        scope.launch {
            val summary = withContextIO {
                val loadErr = NativeBridge.tryLoad()
                when {
                    loadErr != null -> "error: $loadErr"
                    !aiCore.ensureDownloaded() -> "error: on-device model unavailable on this device"
                    else -> {
                        val ws = ctx.filesDir.resolve("workspace").apply { mkdirs() }
                        bridge.runTask(t, ws.absolutePath)
                    }
                }
            }
            summary.lines().filter { it.isNotBlank() }.forEach { log.add(classify(it)) }
            running = false
        }
    }

    // Chat mode: a plain Q&A with the on-device model — no agent loop, no file tools.
    // Just send the message and show the reply (what "what color is a turtle?" should do).
    fun startChat() {
        val t = task.trim(); if (t.isEmpty()) return
        task = ""
        running = true
        log.add(LogLine("you: $t", LogLine.Kind.META))
        scope.launch {
            val reply = withContextIO {
                NativeBridge.tryLoad()  // ensure the lib is loaded (harmless if already)
                if (!aiCore.ensureDownloaded()) "on-device model unavailable on this device"
                else runCatching { aiCore.chat(t) }.getOrElse { "error: ${it.message}" }
            }
            log.add(LogLine(reply, LogLine.Kind.MODEL))
            running = false
        }
    }

    val listState = rememberLazyListState()
    LaunchedEffect(log.size) { if (log.isNotEmpty()) listState.animateScrollToItem(log.size - 1) }

    Scaffold(
        containerColor = Sc.Bg,
        topBar = {
            Column {
                val isConnected = if (mode == Mode.REMOTE) connected
                    else onDeviceStatus.contains("ready")
                TopBar(mode, running, isConnected, statusLine, onMode = { mode = it })
                // Connection row — shown until connected (Remote), then it collapses away.
                if (mode == Mode.REMOTE && !connected) {
                    ConnectRow(urlText, onUrl = { urlText = it }) {
                        val parsed = ScClient.fromUrl(urlText)
                        if (parsed == null) {
                            statusLine = "that URL has no ?k= token"
                        } else {
                            client = ScClient(parsed.first, parsed.second)
                            log.clear(); pending.clear()
                            statusLine = "connecting…"
                            scope.launch {
                                runCatching { client!!.status() }
                                    .onSuccess { connected = true; statusLine = "connected · ${it.workspace}" }
                                    .onFailure { statusLine = "connect failed: ${it.message}" }
                            }
                        }
                    }
                }
                // Current repo bar (Remote, connected): shows which project the desktop is
                // attached to, tappable to switch to a recent one.
                if (mode == Mode.REMOTE && connected) {
                    RepoBar(currentProject, hasRecents = projects.size > 1) { showPicker = true }
                }
                if ((mode == Mode.ON_DEVICE || mode == Mode.CHAT) && onDeviceStatus.isNotBlank()) {
                    Text(
                        onDeviceStatus,
                        color = Sc.FgMuted,
                        fontSize = 12.sp,
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
                    )
                }
            }
        },
        bottomBar = {
            Composer(
                task = task,
                onTask = { task = it },
                running = running,
                canSend = task.isNotBlank() && !running &&
                    (if (mode == Mode.REMOTE) client != null else true),
                onSend = {
                    when (mode) {
                        Mode.REMOTE -> startRemote()
                        Mode.ON_DEVICE -> startOnDevice()
                        Mode.CHAT -> startChat()
                    }
                },
                onStop = {
                    val c = client ?: return@Composer
                    scope.launch { c.cancel() }
                },
                showStop = mode == Mode.REMOTE && running,
            )
        },
    ) { inner ->
        SelectionContainer(
            Modifier.fillMaxSize().padding(inner)
        ) {
            LazyColumn(
                state = listState,
                modifier = Modifier.fillMaxSize().padding(horizontal = 16.dp),
                verticalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                if (log.isEmpty()) {
                    item {
                        Text(
                            when (mode) {
                                Mode.REMOTE -> "Connect to your PC, then chat with your live desktop session."
                                Mode.ON_DEVICE -> "On-device mode. Send a coding task to run it locally."
                                Mode.CHAT -> "Chat with the on-device model. Ask anything."
                            },
                            color = Sc.FgMuted,
                            fontSize = 13.sp,
                            modifier = Modifier.padding(top = 24.dp),
                        )
                    }
                }
                items(pending) { p -> ApprovalCard(p, client, scope) }
                items(log) { line -> LogRow(line) }
            }
        }
    }

    // Project picker dialog (Remote): pick a recent project → the desktop switches to it.
    if (showPicker) {
        ProjectPicker(
            projects = projects,
            current = currentProject,
            onPick = { p ->
                showPicker = false
                val c = client ?: return@ProjectPicker
                scope.launch {
                    val r = c.open(p.path)
                    if (!r.ok) log.add(LogLine("open failed: ${r.detail}", LogLine.Kind.ERR))
                    else { currentProject = p.path; log.clear() }
                }
            },
            onDismiss = { showPicker = false },
        )
    }
}

@Composable
private fun TopBar(mode: Mode, running: Boolean, connected: Boolean, status: String, onMode: (Mode) -> Unit) {
    Column(
        Modifier
            .background(Sc.Surface)
            .windowInsetsPadding(WindowInsets.statusBars),
    ) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Image(
                painter = painterResource(R.drawable.sc_logo),
                contentDescription = "smart-coder",
                modifier = Modifier.size(26.dp).clip(Sc.Shape),
            )
            Spacer(Modifier.width(10.dp))
            ModeChip("Remote", mode == Mode.REMOTE) { onMode(Mode.REMOTE) }
            Spacer(Modifier.width(4.dp))
            ModeChip("On-device", mode == Mode.ON_DEVICE) { onMode(Mode.ON_DEVICE) }
            Spacer(Modifier.width(4.dp))
            ModeChip("Chat", mode == Mode.CHAT) { onMode(Mode.CHAT) }
            Spacer(Modifier.weight(1f))
            // Status dot: green = running, orange = connected/idle, grey = disconnected.
            val dotColor = when {
                running -> Sc.Good
                connected -> Sc.Orange
                else -> Sc.Border
            }
            Box(Modifier.size(9.dp).background(dotColor, RoundedCornerShape(50)))
        }
        // The green dot already conveys connected/running, so the status line is
        // redundant most of the time. Show it ONLY for errors (failed connect, bad
        // token, poll error) so those aren't silent.
        if (status.contains("fail") || status.contains("error") || status.contains("no ?k=")) {
            Text(
                status,
                color = Sc.Bad,
                fontSize = 11.sp,
                modifier = Modifier.padding(start = 12.dp, bottom = 6.dp),
            )
        }
        HorizontalDivider(color = Sc.Border, thickness = 1.dp)
    }
}

@Composable
private fun RepoBar(current: String?, hasRecents: Boolean, onSwitch: () -> Unit) {
    val name = current?.substringAfterLast('/')?.substringAfterLast('\\') ?: "(no project)"
    Row(
        Modifier
            .fillMaxWidth()
            .then(if (hasRecents) Modifier.clickable(onClick = onSwitch) else Modifier)
            .padding(horizontal = 12.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("📁", fontSize = 13.sp)
        Spacer(Modifier.width(6.dp))
        Text(name, color = Sc.Fg, fontSize = 13.sp, fontWeight = FontWeight.Medium)
        if (hasRecents) {
            Spacer(Modifier.weight(1f))
            Text("switch ▾", color = Sc.Accent, fontSize = 12.sp)
        }
    }
}

@Composable
private fun ProjectPicker(
    projects: List<ScClient.Project>,
    current: String?,
    onPick: (ScClient.Project) -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = Sc.Surface,
        titleContentColor = Sc.Fg,
        textContentColor = Sc.Fg,
        title = { Text("Recent projects", color = Sc.Fg, fontSize = 16.sp) },
        text = {
            Column {
                for (p in projects) {
                    val isCurrent = p.path == current
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .then(if (isCurrent) Modifier else Modifier.clickable { onPick(p) })
                            .padding(vertical = 10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(if (isCurrent) "● " else "○ ", color = if (isCurrent) Sc.Good else Sc.FgMuted, fontSize = 13.sp)
                        Column {
                            Text(p.name, color = if (isCurrent) Sc.Good else Sc.Fg, fontSize = 14.sp, fontWeight = FontWeight.Medium)
                            Text(p.path, color = Sc.FgMuted, fontSize = 10.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                }
                if (projects.isEmpty()) Text("No recent projects.", color = Sc.FgMuted, fontSize = 13.sp)
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text("Close", color = Sc.Accent) } },
    )
}

@Composable
private fun ModeChip(label: String, selected: Boolean, onClick: () -> Unit) {
    val bg = if (selected) Sc.Accent.copy(alpha = 0.18f) else Color.Transparent
    val fg = if (selected) Sc.Accent else Sc.FgMuted
    Box(
        Modifier
            .background(bg, Sc.Shape)
            .border(1.dp, if (selected) Sc.Accent else Sc.Border, Sc.Shape)
            .clickable(onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 5.dp),
    ) { Text(label, color = fg, fontSize = 12.sp, fontWeight = FontWeight.Medium) }
}

@Composable
private fun ConnectRow(url: String, onUrl: (String) -> Unit, onConnect: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        TextField(
            value = url,
            onValueChange = onUrl,
            placeholder = { Text("Paste the http://…/?k=… URL", fontSize = 13.sp, color = Sc.FgMuted) },
            singleLine = true,
            modifier = Modifier.weight(1f).heightIn(min = 44.dp),
            textStyle = androidx.compose.ui.text.TextStyle(fontSize = 13.sp, color = Sc.Fg),
            shape = Sc.Shape,
            colors = TextFieldDefaults.colors(
                focusedContainerColor = Sc.InputBg,
                unfocusedContainerColor = Sc.InputBg,
                focusedIndicatorColor = Color.Transparent,
                unfocusedIndicatorColor = Color.Transparent,
                cursorColor = Sc.Accent,
            ),
        )
        Button(
            onClick = onConnect,
            colors = ButtonDefaults.buttonColors(containerColor = Sc.Accent, contentColor = Color(0xFF201007)),
            shape = Sc.Shape,
        ) { Text("Connect") }
    }
}

@Composable
private fun Composer(
    task: String,
    onTask: (String) -> Unit,
    running: Boolean,
    canSend: Boolean,
    onSend: () -> Unit,
    onStop: () -> Unit,
    showStop: Boolean,
) {
    Surface(color = Sc.Surface) {
        Column(
            Modifier
                .imePadding()
                .windowInsetsPadding(WindowInsets.navigationBars),
        ) {
            HorizontalDivider(color = Sc.Border, thickness = 1.dp)
            Row(
                Modifier.fillMaxWidth().padding(8.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                // A compact filled input (BasicTextField-backed TextField has far less
                // internal padding than OutlinedTextField). Fixed 44dp height = tight.
                TextField(
                    value = task,
                    onValueChange = onTask,
                    placeholder = { Text("Message smart-coder…", fontSize = 14.sp, color = Sc.FgMuted) },
                    modifier = Modifier.weight(1f).heightIn(min = 44.dp),
                    maxLines = 4,
                    textStyle = androidx.compose.ui.text.TextStyle(fontSize = 14.sp, color = Sc.Fg),
                    shape = Sc.Shape,
                    colors = TextFieldDefaults.colors(
                        focusedContainerColor = Sc.InputBg,
                        unfocusedContainerColor = Sc.InputBg,
                        focusedIndicatorColor = Color.Transparent,
                        unfocusedIndicatorColor = Color.Transparent,
                        cursorColor = Sc.Accent,
                    ),
                )
                if (showStop) {
                    Box(
                        Modifier
                            .height(44.dp).widthIn(min = 64.dp)
                            .background(Sc.Bad.copy(alpha = 0.18f), Sc.Shape)
                            .border(1.dp, Sc.Bad, Sc.Shape)
                            .clickable(onClick = onStop),
                        contentAlignment = Alignment.Center,
                    ) { Text("Stop", color = Sc.Bad, fontWeight = FontWeight.Bold, fontSize = 14.sp) }
                } else {
                    val on = canSend
                    Box(
                        Modifier
                            .height(44.dp).widthIn(min = 64.dp)
                            .background(if (on) Sc.Orange else Sc.Orange.copy(alpha = 0.35f), Sc.Shape)
                            .then(if (on) Modifier.clickable(onClick = onSend) else Modifier),
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(
                            if (running) "…" else "Send",
                            color = Color(0xFF201007).copy(alpha = if (on) 1f else 0.6f),
                            fontWeight = FontWeight.Bold, fontSize = 14.sp,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun LogRow(line: LogLine) {
    val color = when (line.kind) {
        LogLine.Kind.MODEL -> Sc.Fg
        LogLine.Kind.CALL -> Sc.Accent
        LogLine.Kind.OK -> Sc.Good
        LogLine.Kind.ERR -> Sc.Bad
        LogLine.Kind.META -> Sc.FgMuted
        // Live-typing line: slightly dimmed so it reads as in-progress until finalized.
        LogLine.Kind.STREAM -> Sc.Fg.copy(alpha = 0.7f)
    }
    Text(
        line.text,
        color = color,
        fontFamily = FontFamily.Monospace,
        fontSize = 12.5.sp,
        lineHeight = 17.sp,
    )
}

@Composable
private fun ApprovalCard(
    p: Pending,
    client: ScClient?,
    scope: kotlinx.coroutines.CoroutineScope,
) {
    Column(
        Modifier
            .fillMaxWidth()
            .padding(vertical = 6.dp)
            .background(Color(0xFF2A2410), Sc.Shape)
            .border(1.dp, Sc.Orange.copy(alpha = 0.6f), Sc.Shape)
            .padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("approval needed", color = Sc.Orange, fontSize = 11.sp, fontWeight = FontWeight.Bold)
        Text("$ ${p.command}", color = Sc.Fg, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
        if (p.reason.isNotBlank()) {
            Text(p.reason, color = Sc.FgMuted, fontSize = 11.sp, maxLines = 2, overflow = TextOverflow.Ellipsis)
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = { client ?: return@Button; scope.launch { client.approve(p.id) } },
                colors = ButtonDefaults.buttonColors(containerColor = Sc.Good, contentColor = Color(0xFF07160C)),
                shape = Sc.Shape,
                modifier = Modifier.weight(1f),
            ) { Text("Approve") }
            OutlinedButton(
                onClick = { client ?: return@OutlinedButton; scope.launch { client.deny(p.id) } },
                border = androidx.compose.foundation.BorderStroke(1.dp, Sc.Bad),
                colors = ButtonDefaults.outlinedButtonColors(contentColor = Sc.Bad),
                shape = Sc.Shape,
                modifier = Modifier.weight(1f),
            ) { Text("Deny") }
        }
    }
}

/** Classify an on-device transcript line for coloring (mirrors the Rust append_event format). */
private fun classify(s: String): LogLine {
    val t = s.trimEnd()
    return when {
        t.contains("ERR:") || t.startsWith("error") || t.contains("stalled") ->
            LogLine(t, LogLine.Kind.ERR)
        t.trimStart().startsWith("call ") -> LogLine(t, LogLine.Kind.CALL)
        t.trimStart().startsWith("ok:") -> LogLine(t, LogLine.Kind.OK)
        t.startsWith("[") -> LogLine(t, LogLine.Kind.MODEL)
        else -> LogLine(t, LogLine.Kind.META)
    }
}

/** Fold one remote event into the log, tracking pending approvals so cards appear/clear. */
private fun foldEvent(
    e: ScClient.Event,
    log: MutableList<LogLine>,
    pending: MutableList<Pending>,
    resolved: MutableSet<Long>,
) {
    val o: JSONObject = e.obj
    fun add(text: String, k: LogLine.Kind) = log.add(LogLine(text, k))
    when (e.type) {
        "RunStarted" -> add("● ${o.optString("task")}", LogLine.Kind.META)
        "ToolCall" -> add("▸ ${o.optString("tool")}  ${o.optString("arg")}", LogLine.Kind.CALL)
        "ToolResult" -> {
            val err = o.optBoolean("is_error", false)
            add("${if (err) "✗" else "└"} ${o.optString("summary", o.optString("full"))}",
                if (err) LogLine.Kind.ERR else LogLine.Kind.OK)
        }
        "Verification" -> add(
            "⊨ verify ${if (o.optBoolean("green")) "GREEN" else "RED"}: ${o.optString("summary")}",
            if (o.optBoolean("green")) LogLine.Kind.OK else LogLine.Kind.ERR,
        )
        "Advice" -> add("☎ ${o.optString("advice")}", LogLine.Kind.META)
        "Stalled" -> add("⚠ stalled: ${o.optString("trigger")}", LogLine.Kind.ERR)
        "Stopped" -> add("■ stopped", LogLine.Kind.META)
        "ConfirmPending" -> {
            val id = o.optLong("id")
            if (resolved.add(id) && pending.none { it.id == id }) {
                pending.add(Pending(id, o.optString("command"), o.optString("reason")))
            }
        }
        "ConfirmResolved" -> {
            val id = o.optLong("id")
            pending.removeAll { it.id == id }
        }
        // The live desktop chat, mirrored here. `you`/`agent`/`system` roles.
        "ChatMessage" -> {
            // The final turn lands → drop the transient live-typing line first.
            if (log.lastOrNull()?.kind == LogLine.Kind.STREAM) log.removeAt(log.size - 1)
            val role = o.optString("role")
            val text = o.optString("text")
            val prefix = when (role) { "you" -> "you › "; "agent" -> "‹ "; else -> "" }
            add("$prefix$text", if (role == "you") LogLine.Kind.META else LogLine.Kind.MODEL)
        }
        // Live token stream — replace the transient STREAM line each token (or add it).
        "ChatDelta" -> {
            val line = LogLine("‹ ${o.optString("cumulative")}", LogLine.Kind.STREAM)
            if (log.lastOrNull()?.kind == LogLine.Kind.STREAM) log[log.size - 1] = line
            else log.add(line)
        }
    }
}
