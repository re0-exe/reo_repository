package dev.tablink.display.protocol

import java.io.IOException

/** Wire constants. Must match `host/crates/protocol`. */
object Protocol {
    const val VERSION = 1
    const val PORT = 27183
    const val HEADER_LEN = 14
    const val MAX_PAYLOAD = 16 * 1024 * 1024
    const val FLAG_KEYFRAME = 1
}

/** First byte the host writes on each TCP connection. */
enum class Channel(val id: Int) {
    CONTROL(1), VIDEO(2), AUDIO(3);

    companion object {
        fun fromId(id: Int): Channel? = entries.firstOrNull { it.id == id }
    }
}

internal object MsgType {
    const val HELLO = 0x01
    const val CONFIG = 0x02
    const val POINTER = 0x03
    const val REQUEST_KEYFRAME = 0x04
    const val PING = 0x05
    const val PONG = 0x06
    const val AUDIO_ROUTE = 0x07
    const val VIDEO_CONFIG = 0x10
    const val VIDEO_FRAME = 0x11
    const val AUDIO_FRAME = 0x20
}

class ProtocolException(msg: String) : IOException(msg)
