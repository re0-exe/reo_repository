package dev.tablink.display.protocol

import java.io.DataInputStream
import java.io.InputStream
import java.io.OutputStream
import java.nio.BufferUnderflowException
import java.nio.ByteBuffer
import java.nio.ByteOrder

sealed interface Message {
    /** `caps` bits: 0 HEVC Main, 1 Main10, 2 4:4:4, 3 HDR10. */
    data class Hello(
        val version: Int, val width: Int, val height: Int,
        val refreshHz: List<Int>, val caps: Int, val dpi: Int,
    ) : Message

    /** `codec`: 0 Main 4:2:0, 1 Main10, 2 4:4:4. `profile`: 0 Draw, 1 Video. */
    data class Config(
        val width: Int, val height: Int, val fps: Int, val codec: Int,
        val hdr: Boolean, val profile: Int, val bitrateKbps: Int,
    ) : Message

    data object RequestKeyframe : Message
    data object Ping : Message
    data object Pong : Message

    /** A frame left the decoder; the header's pts is that frame's. */
    data object FrameAck : Message

    /** VPS+SPS+PPS in Annex-B. */
    class VideoConfig(val data: ByteArray) : Message
    class VideoFrame(val keyframe: Boolean, val data: ByteArray) : Message
}

private val EMPTY = ByteArray(0)

private fun le(size: Int): ByteBuffer = ByteBuffer.allocate(size).order(ByteOrder.LITTLE_ENDIAN)

fun writeMessage(out: OutputStream, ptsUs: Long, m: Message) {
    var flags = 0
    val type: Int
    val payload: ByteArray
    when (m) {
        is Message.Hello -> {
            type = MsgType.HELLO
            require(m.refreshHz.size <= 255) { "too many refresh rates" }
            val b = le(17 + 2 * m.refreshHz.size)
            b.putShort(m.version.toShort()).putInt(m.width).putInt(m.height)
            b.put(m.refreshHz.size.toByte())
            m.refreshHz.forEach { b.putShort(it.toShort()) }
            b.putInt(m.caps).putShort(m.dpi.toShort())
            payload = b.array()
        }
        is Message.Config -> {
            type = MsgType.CONFIG
            payload = le(17)
                .putInt(m.width).putInt(m.height).putShort(m.fps.toShort())
                .put(m.codec.toByte()).put((if (m.hdr) 1 else 0).toByte()).put(m.profile.toByte())
                .putInt(m.bitrateKbps).array()
        }
        Message.RequestKeyframe -> { type = MsgType.REQUEST_KEYFRAME; payload = EMPTY }
        Message.Ping -> { type = MsgType.PING; payload = EMPTY }
        Message.Pong -> { type = MsgType.PONG; payload = EMPTY }
        Message.FrameAck -> { type = MsgType.FRAME_ACK; payload = EMPTY }
        is Message.VideoConfig -> { type = MsgType.VIDEO_CONFIG; payload = m.data }
        is Message.VideoFrame -> {
            type = MsgType.VIDEO_FRAME
            if (m.keyframe) flags = flags or Protocol.FLAG_KEYFRAME
            payload = m.data
        }
    }
    require(payload.size <= Protocol.MAX_PAYLOAD) { "payload exceeds MAX_PAYLOAD" }
    val header = le(Protocol.HEADER_LEN)
        .put(type.toByte()).put(flags.toByte()).putInt(payload.size).putLong(ptsUs).array()
    // One write per message keeps header and payload in the same TCP segment where possible.
    out.write(header + payload)
    out.flush()
}

/**
 * Reads the next message this build understands; message types reserved for
 * later phases are skipped. Throws [java.io.EOFException] when the stream ends
 * (including mid-message) and [ProtocolException] on malformed input.
 */
fun readMessage(input: InputStream): Pair<Long, Message> {
    val din = input as? DataInputStream ?: DataInputStream(input)
    while (true) {
        val hb = ByteArray(Protocol.HEADER_LEN)
        din.readFully(hb)
        val h = ByteBuffer.wrap(hb).order(ByteOrder.LITTLE_ENDIAN)
        val type = h.get().toInt() and 0xFF
        val flags = h.get().toInt() and 0xFF
        val length = h.getInt()
        val ptsUs = h.getLong()
        if (type !in KNOWN_TYPES) throw ProtocolException("unknown message type 0x%02x".format(type))
        // A length above 2^31 reads as negative; both cases are oversize.
        if (length < 0 || length > Protocol.MAX_PAYLOAD) throw ProtocolException("payload too large")
        val payload = ByteArray(length)
        din.readFully(payload)
        val msg: Message = when (type) {
            MsgType.HELLO -> parse(payload, ::parseHello)
            MsgType.CONFIG -> parse(payload, ::parseConfig)
            MsgType.REQUEST_KEYFRAME -> Message.RequestKeyframe
            MsgType.PING -> Message.Ping
            MsgType.PONG -> Message.Pong
            MsgType.FRAME_ACK -> Message.FrameAck
            MsgType.VIDEO_CONFIG -> Message.VideoConfig(payload)
            MsgType.VIDEO_FRAME -> Message.VideoFrame(flags and Protocol.FLAG_KEYFRAME != 0, payload)
            else -> continue
        }
        return ptsUs to msg
    }
}

private val KNOWN_TYPES = setOf(
    MsgType.HELLO, MsgType.CONFIG, MsgType.POINTER, MsgType.REQUEST_KEYFRAME, MsgType.PING,
    MsgType.PONG, MsgType.AUDIO_ROUTE, MsgType.FRAME_ACK, MsgType.VIDEO_CONFIG, MsgType.VIDEO_FRAME, MsgType.AUDIO_FRAME,
)

private fun <T> parse(payload: ByteArray, f: (ByteBuffer) -> T): T = try {
    f(ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN))
} catch (e: BufferUnderflowException) {
    throw ProtocolException("message payload is truncated")
}

private fun ByteBuffer.u16(): Int = getShort().toInt() and 0xFFFF
private fun ByteBuffer.u8(): Int = get().toInt() and 0xFF

private fun parseHello(b: ByteBuffer): Message.Hello {
    val version = b.u16()
    if (version != Protocol.VERSION) throw ProtocolException("unsupported protocol version $version")
    val width = b.getInt()
    val height = b.getInt()
    val refresh = List(b.u8()) { b.u16() }
    return Message.Hello(version, width, height, refresh, b.getInt(), b.u16())
}

private fun parseConfig(b: ByteBuffer): Message.Config = Message.Config(
    width = b.getInt(), height = b.getInt(), fps = b.u16(), codec = b.u8(),
    hdr = b.u8() != 0, profile = b.u8(), bitrateKbps = b.getInt(),
)
