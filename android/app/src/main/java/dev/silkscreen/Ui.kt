package dev.silkscreen

import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView

private val Accent = Color(0xFF6EA8FE)
private val Good = Color(0xFF78C88C)
private val Warn = Color(0xFFF0BE5A)
private val Muted = Color(0xFF9AA0A6)

@Composable
fun SilkscreenApp(activity: MainActivity) {
    MaterialTheme(colorScheme = darkColorScheme(primary = Accent)) {
        val m = activity.model
        if (m.displayMode) DisplayScreen(activity) else HomeScreen(activity)
    }
}

// ---------------------------------------------------------------------------
// Home

@Composable
private fun HomeScreen(activity: MainActivity) {
    val m = activity.model
    val info = m.info
    Column(
        Modifier
            .fillMaxSize()
            .background(Color(0xFF1B1B1B))
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Silkscreen", color = Accent, fontSize = 30.sp, fontWeight = FontWeight.Light)

        var name by remember(info.optString("name")) { mutableStateOf(info.optString("name")) }
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text("This device's name") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            if (name.isNotBlank() && name != info.optString("name")) {
                Button(onClick = { Native.setName(name.trim()); activity.refreshInfo() }) { Text("Save") }
            }
        }
        Text(info.optString("fingerprint"), color = Muted, fontFamily = FontFamily.Monospace)

        Section("Use this device as a display") {
            Text("Turn this screen into a wireless monitor that a Windows, macOS or Linux computer running Silkscreen can extend onto — with touch, stylus and keyboard.")
            KeepAwakeSetting(activity)
            m.error?.let { Text(it, color = Warn) }
            Button(onClick = { activity.startDisplay() }, modifier = Modifier.fillMaxWidth().height(52.dp)) {
                Text("Start display mode", fontSize = 18.sp)
            }
            Text(
                "While a computer is showing, press Back for options. On a keyboard, Ctrl+Alt+Shift+G disconnects.",
                color = Muted,
                fontSize = 13.sp,
            )
        }

        Section("Paired computers") {
            val paired = info.optJSONArray("paired")
            if (paired == null || paired.length() == 0) {
                Text("None yet. The first time a computer connects, this screen shows a code to type on it.", color = Muted)
            } else {
                for (i in 0 until paired.length()) {
                    val p = paired.getJSONObject(i)
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(p.optString("name"), fontWeight = FontWeight.Medium)
                            Text(p.optString("fingerprint"), color = Muted, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
                        }
                        TextButton(onClick = { Native.forget(p.optString("key")); activity.refreshInfo() }) { Text("Forget") }
                    }
                }
            }
        }

        Section("Log") {
            LogText(m.logs, maxLines = 40)
        }
    }
}

@Composable
private fun Section(title: String, content: @Composable () -> Unit) {
    Card(
        colors = CardDefaults.cardColors(containerColor = Color(0xFF242424), contentColor = Color(0xFFE8E8E8)),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(title, fontSize = 18.sp, fontWeight = FontWeight.Medium)
            content()
        }
    }
}

@Composable
private fun KeepAwakeSetting(activity: MainActivity) {
    val info = activity.model.info
    val mode = info.optInt("keepAwake", 0)
    val minutes = info.optInt("idleMinutes", 10)
    val labels = listOf("While a computer is connected", "While the picture is changing", "Never (use system settings)")
    var open by remember { mutableStateOf(false) }
    Column {
        Text("Keep this screen awake", color = Muted, fontSize = 13.sp)
        Box {
            OutlinedButton(onClick = { open = true }) { Text(labels[mode.coerceIn(0, 2)]) }
            DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                labels.forEachIndexed { i, label ->
                    DropdownMenuItem(text = { Text(label) }, onClick = {
                        open = false
                        Native.setKeepAwake(i, minutes)
                        activity.refreshInfo()
                    })
                }
            }
        }
        if (mode == 1) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Then let it sleep after $minutes min without changes")
                TextButton(onClick = { Native.setKeepAwake(1, (minutes - 5).coerceAtLeast(1)); activity.refreshInfo() }) { Text("−") }
                TextButton(onClick = { Native.setKeepAwake(1, minutes + 5); activity.refreshInfo() }) { Text("+") }
            }
        }
    }
}

@Composable
private fun LogText(logs: String, maxLines: Int) {
    val lines = logs.lines().takeLast(maxLines).joinToString("\n")
    Text(
        lines.ifBlank { "(empty)" },
        fontFamily = FontFamily.Monospace,
        fontSize = 11.sp,
        color = Color(0xFFCCCCCC),
        lineHeight = 14.sp,
    )
}

// ---------------------------------------------------------------------------
// Display mode

@Composable
private fun DisplayScreen(activity: MainActivity) {
    val m = activity.model
    val s = m.state
    val w = s.optInt("videoWidth")
    val h = s.optInt("videoHeight")
    Box(Modifier.fillMaxSize().background(Color.Black), contentAlignment = Alignment.Center) {
        // The surface exists for the whole of display mode, so the decoder
        // has somewhere to draw the moment a stream starts. It takes the
        // video's aspect ratio so touches map 1:1 onto the host's screen.
        val sized = if (m.streaming && w > 0 && h > 0) Modifier.aspectRatio(w.toFloat() / h) else Modifier.fillMaxSize()
        AndroidView(factory = { ctx -> videoSurface(ctx) }, modifier = sized)

        if (!m.streaming) WaitingCard(activity)
        if (m.debug) DebugOverlay(m, Modifier.align(Alignment.TopStart))
        if (m.menuOpen) Menu(activity)
    }
}

private fun videoSurface(ctx: android.content.Context): SurfaceView = SurfaceView(ctx).apply {
    holder.addCallback(object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) = Native.setSurface(holder.surface)
        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
        override fun surfaceDestroyed(holder: SurfaceHolder) = Native.setSurface(null)
    })
    val input = VideoInput(this)
    setOnTouchListener(input)
    setOnGenericMotionListener(input)
    isFocusable = true
}

@Composable
private fun WaitingCard(activity: MainActivity) {
    val m = activity.model
    val s = m.state
    Column(
        Modifier.fillMaxSize().background(Color(0xFF0E1016)).padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text(m.info.optString("name"), fontSize = 40.sp, color = Color.White)
        Spacer(Modifier.height(16.dp))
        if (m.phase == "pairing") {
            Text("\"${s.optString("host")}\" wants to use this display", fontSize = 20.sp, color = Color.White, textAlign = TextAlign.Center)
            Text("Enter this code on that computer:", color = Muted, fontSize = 16.sp)
            val code = s.optString("code")
            Text(
                code.take(3) + " " + code.drop(3),
                fontSize = 84.sp,
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Bold,
                color = Accent,
            )
        } else {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                CircularProgressIndicator(Modifier.height(22.dp).widthIn(max = 22.dp), strokeWidth = 2.dp)
                Text("Ready — waiting for a computer", fontSize = 20.sp, color = Color.White)
            }
            Text("On the other computer, open Silkscreen and pick this display.", color = Muted, textAlign = TextAlign.Center)
            s.optString("notice").takeIf { it.isNotBlank() && it != "null" }?.let {
                Spacer(Modifier.height(12.dp))
                Text(it, color = Warn, textAlign = TextAlign.Center)
            }
        }
        Spacer(Modifier.height(20.dp))
        Text(
            listOfNotNull(s.optString("address").takeIf { it.isNotBlank() && it != "null" }, m.info.optString("fingerprint"))
                .joinToString("    "),
            color = Muted,
            fontFamily = FontFamily.Monospace,
        )
        Spacer(Modifier.height(20.dp))
        Text("Press Back for options", color = Muted, fontSize = 13.sp)
    }
}

@Composable
private fun Menu(activity: MainActivity) {
    val m = activity.model
    Box(Modifier.fillMaxSize().background(Color(0xAA000000)), contentAlignment = Alignment.Center) {
        Column(
            Modifier
                .widthIn(min = 280.dp, max = 440.dp)
                .background(Color(0xFF242424), RoundedCornerShape(16.dp))
                .padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text("Silkscreen", fontSize = 20.sp, color = Accent)
            if (m.streaming) {
                Text("Showing \"${m.state.optString("host")}\"", color = Muted)
                Button(onClick = { activity.disconnect() }, modifier = Modifier.fillMaxWidth()) { Text("Disconnect") }
            }
            OutlinedButton(onClick = { m.debug = !m.debug; m.menuOpen = false }, modifier = Modifier.fillMaxWidth()) {
                Text(if (m.debug) "Hide debug info" else "Show debug info")
            }
            OutlinedButton(onClick = { activity.stopDisplay() }, modifier = Modifier.fillMaxWidth()) {
                Text("Leave display mode")
            }
            TextButton(onClick = { m.menuOpen = false }) { Text("Close") }
        }
    }
}

@Composable
private fun DebugOverlay(m: Model, modifier: Modifier) {
    val s = m.state
    Column(
        modifier
            .safeDrawingPadding()
            .padding(12.dp)
            .background(Color(0xD0000000), RoundedCornerShape(8.dp))
            .padding(12.dp)
            .widthIn(max = 640.dp)
            .heightIn(max = 480.dp),
    ) {
        Text("Silkscreen debug", color = Accent)
        val rows = listOf(
            "State" to (s.optString("phase") + (s.optString("host").takeIf { it.isNotBlank() && it != "null" }?.let { " · $it" } ?: "")),
            "Stream" to s.optString("stream", "–"),
            "Decoder" to s.optString("decoder", "–"),
            "Frame rate" to "%.1f fps".format(s.optDouble("fps", 0.0)),
            "Receive to decoder" to "%.2f ms".format(s.optDouble("latencyMs", 0.0)),
            "Dropped / lost" to "${s.optInt("dropped")} frames · ${s.optInt("lost")} packets",
        )
        for ((k, v) in rows) {
            Row { Text(k, color = Muted, modifier = Modifier.widthIn(min = 150.dp), fontSize = 13.sp); Text(v, color = Color.White, fontSize = 13.sp) }
        }
        Spacer(Modifier.height(8.dp))
        LogText(m.logs, maxLines = 14)
    }
}
