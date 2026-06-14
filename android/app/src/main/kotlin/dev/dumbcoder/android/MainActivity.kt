/*
 * MainActivity — the thinnest possible shell: a button that hands a task to the
 * Rust agent core (which drives AICore for inference), and a text view for the
 * result.
 *
 * STATUS: REFERENCE — NOT COMPILED OR TESTED HERE (no Android SDK/device in CI).
 * Plain `Activity` + programmatic views to avoid extra UI dependencies; swap for
 * Compose/AppCompat as you build the real client (spec 12).
 */
package dev.dumbcoder.android

import android.app.Activity
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import java.io.File

class MainActivity : Activity() {

    private val bridge by lazy { NativeBridge(AiCoreBackend()) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val output = TextView(this).apply { text = "dumb-coder (on-device)\n" }
        val run = Button(this).apply { text = "Run sample task" }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(run)
            addView(ScrollView(this@MainActivity).apply { addView(output) })
        }
        setContentView(root)

        run.setOnClickListener {
            output.text = "running…"
            // Off the main thread: the JNI up-call into onGenerate() runs AICore
            // synchronously on this worker thread (spec 12).
            Thread {
                // App-scoped working directory — Android has no arbitrary shell and
                // only scoped storage (spec 12); the agent operates inside here.
                val workspace = File(filesDir, "workspace").apply { mkdirs() }
                val summary = try {
                    bridge.runTask(
                        "Create a file hello.txt containing the text hi.",
                        workspace.absolutePath,
                    )
                } catch (t: Throwable) {
                    "error: ${t.message}"
                }
                runOnUiThread { output.text = summary }
            }.start()
        }
    }
}
