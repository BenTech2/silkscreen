package dev.silkscreen

import android.view.MotionEvent
import android.view.View
import kotlin.math.atan
import kotlin.math.cos
import kotlin.math.sin
import kotlin.math.tan

/**
 * Turns touches, stylus, mouse and scroll on the video surface into input
 * for the host. The surface has the video's aspect ratio, so view
 * coordinates divided by its size are the host's normalised coordinates.
 */
class VideoInput(private val view: View) : View.OnTouchListener, View.OnGenericMotionListener {
    private var mouseButtons = 0
    private var penDown = false

    private fun nx(e: MotionEvent, i: Int) = (e.getX(i) / view.width).coerceIn(0f, 1f)
    private fun ny(e: MotionEvent, i: Int) = (e.getY(i) / view.height).coerceIn(0f, 1f)

    override fun onTouch(v: View, e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> pointer(e, e.actionIndex, PHASE_DOWN)
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> pointer(e, e.actionIndex, PHASE_UP)
            MotionEvent.ACTION_MOVE -> for (i in 0 until e.pointerCount) pointer(e, i, PHASE_MOVE)
            MotionEvent.ACTION_CANCEL -> for (i in 0 until e.pointerCount) pointer(e, i, PHASE_CANCEL)
            MotionEvent.ACTION_BUTTON_PRESS, MotionEvent.ACTION_BUTTON_RELEASE -> buttons(e)
        }
        return true
    }

    private fun pointer(e: MotionEvent, i: Int, phase: Int) {
        when (e.getToolType(i)) {
            MotionEvent.TOOL_TYPE_STYLUS, MotionEvent.TOOL_TYPE_ERASER -> pen(e, i, phase, hover = false)
            MotionEvent.TOOL_TYPE_MOUSE -> {
                Native.mouseMove(nx(e, i), ny(e, i))
                buttons(e)
            }
            else -> Native.touch(e.getPointerId(i), phase, nx(e, i), ny(e, i), e.getPressure(i))
        }
    }

    private fun pen(e: MotionEvent, i: Int, phase: Int, hover: Boolean) {
        // Android reports tilt from vertical plus an orientation; the host
        // wants tilt along x and y in degrees.
        val tilt = e.getAxisValue(MotionEvent.AXIS_TILT, i)
        val orientation = e.getAxisValue(MotionEvent.AXIS_ORIENTATION, i)
        val t = tan(tilt)
        val tiltX = Math.toDegrees(atan(t * sin(orientation)).toDouble()).toFloat()
        val tiltY = Math.toDegrees(atan(-t * cos(orientation)).toDouble()).toFloat()
        val contact = !hover && phase != PHASE_UP && phase != PHASE_CANCEL
        penDown = contact
        Native.pen(
            phase,
            nx(e, i),
            ny(e, i),
            if (contact) e.getPressure(i) else 0f,
            tiltX,
            tiltY,
            barrel = e.buttonState and MotionEvent.BUTTON_STYLUS_PRIMARY != 0,
            eraser = e.getToolType(i) == MotionEvent.TOOL_TYPE_ERASER,
            hover = hover,
        )
    }

    private fun buttons(e: MotionEvent) {
        val now = e.buttonState
        val changed = now xor mouseButtons
        for ((mask, button) in BUTTONS) {
            if (changed and mask != 0) Native.mouseButton(button, now and mask != 0)
        }
        mouseButtons = now
    }

    override fun onGenericMotion(v: View, e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> when (e.getToolType(0)) {
                MotionEvent.TOOL_TYPE_STYLUS, MotionEvent.TOOL_TYPE_ERASER -> pen(e, 0, PHASE_MOVE, hover = true)
                else -> Native.mouseMove(nx(e, 0), ny(e, 0))
            }
            MotionEvent.ACTION_HOVER_EXIT -> if (e.getToolType(0) == MotionEvent.TOOL_TYPE_STYLUS && !penDown) {
                pen(e, 0, PHASE_CANCEL, hover = true)
            }
            // Both use "positive = up/left", like the protocol.
            MotionEvent.ACTION_SCROLL -> Native.scroll(
                -e.getAxisValue(MotionEvent.AXIS_HSCROLL),
                e.getAxisValue(MotionEvent.AXIS_VSCROLL),
            )
            else -> return false
        }
        return true
    }

    companion object {
        const val PHASE_DOWN = 0
        const val PHASE_MOVE = 1
        const val PHASE_UP = 2
        const val PHASE_CANCEL = 3

        private val BUTTONS = listOf(
            MotionEvent.BUTTON_PRIMARY to 0,
            MotionEvent.BUTTON_SECONDARY to 1,
            MotionEvent.BUTTON_TERTIARY to 2,
            MotionEvent.BUTTON_BACK to 3,
            MotionEvent.BUTTON_FORWARD to 4,
        )
    }
}
