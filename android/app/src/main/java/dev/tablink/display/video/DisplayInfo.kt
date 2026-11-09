package dev.tablink.display.video

import dev.tablink.display.protocol.Message
import dev.tablink.display.protocol.Protocol
import kotlin.math.roundToInt

/** What the host needs to know about this panel to create a matching monitor. */
data class DisplayInfo(val width: Int, val height: Int, val refreshHz: List<Int>, val dpi: Int)

/** One mode the panel reports, as plain values so the selection is testable. */
data class PanelMode(val width: Int, val height: Int, val refreshHz: Float)

/** The stream is always landscape; the activity is locked to that orientation. */
fun landscape(info: DisplayInfo): DisplayInfo =
    if (info.width >= info.height) info else info.copy(width = info.height, height = info.width)

/**
 * Native resolution is the largest mode the panel offers; the refresh rates
 * are those available at that resolution.
 */
fun displayInfoOf(modes: List<PanelMode>, dpi: Int): DisplayInfo {
    val native = modes.maxByOrNull { it.width.toLong() * it.height } ?: return DisplayInfo(0, 0, emptyList(), dpi)
    val rates = modes
        .filter { it.width == native.width && it.height == native.height }
        .map { it.refreshHz.roundToInt() }
        .filter { it > 0 }
        .distinct()
        .sorted()
    return landscape(DisplayInfo(native.width, native.height, rates, dpi))
}

fun buildHello(metrics: DisplayInfo, decoderCaps: Int): Message.Hello =
    Message.Hello(Protocol.VERSION, metrics.width, metrics.height, metrics.refreshHz, decoderCaps, metrics.dpi)

/** Bits of `Hello.caps`. */
object DecoderCaps {
    const val HEVC_MAIN = 1 shl 0
    const val HEVC_MAIN10 = 1 shl 1
    const val HEVC_444 = 1 shl 2
    const val HDR10 = 1 shl 3
}
