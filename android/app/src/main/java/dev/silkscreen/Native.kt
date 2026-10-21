package dev.silkscreen

import android.view.Surface

/**
 * The Rust core (`crates/silkscreen-android`): pairing, encryption, the
 * protocol, discovery and MediaCodec decoding. State comes back as JSON.
 */
object Native {
    init {
        System.loadLibrary("silkscreen_android")
    }

    @JvmStatic external fun init(filesDir: String, defaultName: String)

    /** `{name, fingerprint, keepAwake, idleMinutes, paired: [{name, key, fingerprint}]}` */
    @JvmStatic external fun info(): String
    @JvmStatic external fun setName(name: String)
    @JvmStatic external fun forget(key: String)

    /** mode: 0 = while connected, 1 = while the picture changes, 2 = never. */
    @JvmStatic external fun setKeepAwake(mode: Int, minutes: Int)

    /** Returns an error message, or null when display mode started. */
    @JvmStatic external fun startDisplay(
        width: Int,
        height: Int,
        refreshMhz: Int,
        scale: Float,
        touchPoints: Int,
        codecs: String,
    ): String?

    @JvmStatic external fun stopDisplay()
    @JvmStatic external fun disconnect()
    @JvmStatic external fun setSurface(surface: Surface?)

    /** `{phase, host, code, stream, decoder, fps, latencyMs, dropped, lost, videoWidth, videoHeight, secondsSinceFrame, address}` */
    @JvmStatic external fun state(): String
    @JvmStatic external fun logs(): String

    /** phase: 0 down, 1 move, 2 up, 3 cancel. Coordinates are 0..1 over the video. */
    @JvmStatic external fun touch(id: Int, phase: Int, x: Float, y: Float, pressure: Float)
    @JvmStatic external fun pen(
        phase: Int,
        x: Float,
        y: Float,
        pressure: Float,
        tiltX: Float,
        tiltY: Float,
        barrel: Boolean,
        eraser: Boolean,
        hover: Boolean,
    )
    @JvmStatic external fun mouseMove(x: Float, y: Float)

    /** button: 0 left, 1 right, 2 middle, 3 back, 4 forward. */
    @JvmStatic external fun mouseButton(button: Int, pressed: Boolean)
    @JvmStatic external fun scroll(dx: Float, dy: Float)

    /** A hardware key by Linux scancode (KeyEvent.getScanCode()). */
    @JvmStatic external fun key(scancode: Int, pressed: Boolean)
}
