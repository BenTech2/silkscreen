package dev.silkscreen

import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.media.MediaCodecList
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONObject

/** Everything the UI shows, refreshed from the Rust core a few times a second. */
class Model {
    var info by mutableStateOf(JSONObject())
    var state by mutableStateOf(JSONObject())
    var logs by mutableStateOf("")
    var displayMode by mutableStateOf(false)
    var menuOpen by mutableStateOf(false)
    var debug by mutableStateOf(false)
    var error by mutableStateOf<String?>(null)

    val phase: String get() = state.optString("phase", "off")
    val streaming: Boolean get() = phase == "streaming"
}

class MainActivity : ComponentActivity() {
    val model = Model()
    private var multicastLock: WifiManager.MulticastLock? = null
    /** Scancodes forwarded as pressed, so they can be released. */
    private val keysDown = mutableSetOf<Int>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        window.attributes.layoutInDisplayCutoutMode =
            WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
        Native.init(filesDir.path, defaultName())
        refreshInfo()

        // Back opens the in-display menu (there is no other on-screen UI
        // while streaming); from the menu or the home screen it goes back.
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                when {
                    model.displayMode && !model.menuOpen -> model.menuOpen = true
                    model.displayMode -> model.menuOpen = false
                    else -> finish()
                }
            }
        })

        setContent { SilkscreenApp(this) }

        lifecycleScope.launch {
            while (isActive) {
                model.state = JSONObject(Native.state())
                if (model.debug || !model.displayMode) model.logs = Native.logs()
                updateKeepAwake()
                if (!model.streaming) releaseKeys()
                delay(250)
            }
        }
    }

    fun refreshInfo() {
        model.info = JSONObject(Native.info())
    }

    private fun defaultName(): String {
        val name = android.provider.Settings.Global.getString(contentResolver, "device_name")
        return name?.takeIf { it.isNotBlank() } ?: "${Build.MANUFACTURER} ${Build.MODEL}".trim()
    }

    // -- display mode --------------------------------------------------------

    fun startDisplay() {
        val bounds = windowManager.maximumWindowMetrics.bounds
        val refresh = display?.refreshRate ?: 60f
        val error = Native.startDisplay(
            bounds.width(),
            bounds.height(),
            (refresh * 1000).toInt(),
            resources.displayMetrics.density,
            touchPoints(),
            decoders(),
        )
        if (error != null) {
            model.error = error
            return
        }
        model.error = null
        model.displayMode = true
        model.menuOpen = false
        // Hosts find us via mDNS, which Android filters without this.
        multicastLock = (applicationContext.getSystemService(WIFI_SERVICE) as WifiManager)
            .createMulticastLock("silkscreen").apply {
                setReferenceCounted(false)
                acquire()
            }
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LOCKED
        immersive(true)
    }

    fun stopDisplay() {
        releaseKeys()
        Native.stopDisplay()
        model.displayMode = false
        model.menuOpen = false
        multicastLock?.release()
        multicastLock = null
        requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
        immersive(false)
        window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }

    fun disconnect() {
        releaseKeys()
        Native.disconnect()
        model.menuOpen = false
    }

    private fun immersive(on: Boolean) {
        val c = WindowCompat.getInsetsController(window, window.decorView)
        if (on) {
            c.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            c.hide(WindowInsetsCompat.Type.systemBars())
        } else {
            c.show(WindowInsetsCompat.Type.systemBars())
        }
    }

    private fun touchPoints(): Int = when {
        packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN_MULTITOUCH_JAZZHAND) -> 10
        packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN_MULTITOUCH_DISTINCT) -> 2
        packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN) -> 1
        else -> 0
    }

    /** Codecs with a decoder here, hardware ones first; HEVC preferred. */
    private fun decoders(): String {
        val infos = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { !it.isEncoder }
        fun has(mime: String, hardware: Boolean) = infos.any { info ->
            info.supportedTypes.any { it.equals(mime, ignoreCase = true) } &&
                (!hardware || info.isHardwareAccelerated)
        }
        val wanted = listOf("hevc" to "video/hevc", "h264" to "video/avc", "av1" to "video/av01")
        val hw = wanted.filter { has(it.second, true) }.map { it.first }
        val sw = wanted.filter { has(it.second, false) && it.first !in hw }.map { it.first }
        return (hw + sw).joinToString(",")
    }

    /** Keeps the screen on per the "keep this screen awake" setting. */
    private fun updateKeepAwake() {
        val info = model.info
        val idle = model.state.optDouble("secondsSinceFrame", Double.MAX_VALUE)
        val keep = model.streaming && when (info.optInt("keepAwake", 0)) {
            0 -> true
            1 -> idle < info.optInt("idleMinutes", 10) * 60.0
            else -> false
        }
        val flag = WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON
        val has = window.attributes.flags and flag != 0
        if (keep && !has) window.addFlags(flag) else if (!keep && has) window.clearFlags(flag)
    }

    // -- keyboard ------------------------------------------------------------

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (!model.displayMode) return super.dispatchKeyEvent(event)
        val hotkey = event.isShiftPressed && event.isAltPressed && (event.isCtrlPressed || event.isMetaPressed)
        if (hotkey && (event.keyCode == KeyEvent.KEYCODE_G || event.keyCode == KeyEvent.KEYCODE_D)) {
            if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) {
                if (event.keyCode == KeyEvent.KEYCODE_D) {
                    model.debug = !model.debug
                } else if (model.phase == "waiting") {
                    stopDisplay()
                } else {
                    disconnect()
                }
            }
            return true
        }
        // Navigation back (gesture/button) has no scancode: it opens the menu.
        val scancode = event.scanCode
        if (!model.streaming || model.menuOpen || scancode <= 0) return super.dispatchKeyEvent(event)
        when (event.action) {
            KeyEvent.ACTION_DOWN -> if (event.repeatCount == 0) {
                keysDown += scancode
                Native.key(scancode, true)
            }
            KeyEvent.ACTION_UP -> if (keysDown.remove(scancode)) Native.key(scancode, false)
        }
        return true
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus) releaseKeys()
        if (hasFocus && model.displayMode) immersive(true)
    }

    private fun releaseKeys() {
        for (k in keysDown) Native.key(k, false)
        keysDown.clear()
    }

    override fun onDestroy() {
        if (isFinishing && model.displayMode) stopDisplay()
        super.onDestroy()
    }
}
