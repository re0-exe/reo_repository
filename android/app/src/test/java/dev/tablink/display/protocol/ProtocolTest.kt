package dev.tablink.display.protocol

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.EOFException
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder

class ProtocolTest {
    private val hello = Message.Hello(1, 2800, 1752, listOf(60, 120), 3, 266)
    private val config = Message.Config(2800, 1752, 120, 0, false, 0, 80_000)

    private fun vector(name: String): ByteArray = File(System.getProperty("vectorsDir"), name).readBytes()

    private fun encode(ptsUs: Long, m: Message): ByteArray =
        ByteArrayOutputStream().also { writeMessage(it, ptsUs, m) }.toByteArray()

    private fun header(type: Int, length: Int): ByteArray =
        ByteBuffer.allocate(Protocol.HEADER_LEN).order(ByteOrder.LITTLE_ENDIAN)
            .put(type.toByte()).put(0.toByte()).putInt(length).putLong(0).array()

    @Test
    fun goldenVectorsDecode() {
        assertEquals(0L to hello, readMessage(ByteArrayInputStream(vector("hello.bin"))))
        assertEquals(0L to config, readMessage(ByteArrayInputStream(vector("config.bin"))))
        assertEquals(0L to Message.Ping, readMessage(ByteArrayInputStream(vector("ping.bin"))))

        val (_, vc) = readMessage(ByteArrayInputStream(vector("video_config.bin")))
        assertArrayEquals(byteArrayOf(0, 0, 0, 1, 0x40), (vc as Message.VideoConfig).data)

        val (pts, vf) = readMessage(ByteArrayInputStream(vector("video_frame_key.bin")))
        vf as Message.VideoFrame
        assertEquals(1000L, pts)
        assertTrue(vf.keyframe)
        assertArrayEquals(byteArrayOf(1, 2, 3), vf.data)
    }

    @Test
    fun goldenVectorsEncode() {
        assertArrayEquals(vector("hello.bin"), encode(0, hello))
        assertArrayEquals(vector("config.bin"), encode(0, config))
        assertArrayEquals(vector("ping.bin"), encode(0, Message.Ping))
        assertArrayEquals(vector("video_config.bin"), encode(0, Message.VideoConfig(byteArrayOf(0, 0, 0, 1, 0x40))))
        assertArrayEquals(vector("video_frame_key.bin"), encode(1000, Message.VideoFrame(true, byteArrayOf(1, 2, 3))))
    }

    @Test
    fun rejectsOversizePayload() {
        val bytes = header(0x11, Protocol.MAX_PAYLOAD + 1)
        assertThrows(ProtocolException::class.java) { readMessage(ByteArrayInputStream(bytes)) }
    }

    @Test
    fun rejectsLengthAboveSignedRange() {
        val bytes = header(0x11, -1)
        assertThrows(ProtocolException::class.java) { readMessage(ByteArrayInputStream(bytes)) }
    }

    @Test
    fun rejectsUnknownType() {
        assertThrows(ProtocolException::class.java) { readMessage(ByteArrayInputStream(header(0xFF, 0))) }
    }

    @Test
    fun truncatedStreamThrowsEof() {
        val bytes = encode(0, Message.VideoFrame(true, byteArrayOf(1, 2, 3))).dropLast(1).toByteArray()
        assertThrows(EOFException::class.java) { readMessage(ByteArrayInputStream(bytes)) }
    }

    @Test
    fun skipsUnhandledType() {
        val bytes = header(0x03, 3) + byteArrayOf(9, 9, 9) + encode(0, Message.Ping)
        assertEquals(Message.Ping, readMessage(ByteArrayInputStream(bytes)).second)
    }
}
