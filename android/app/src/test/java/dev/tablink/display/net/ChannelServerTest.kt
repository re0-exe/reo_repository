package dev.tablink.display.net

import dev.tablink.display.protocol.Channel
import dev.tablink.display.protocol.Message
import dev.tablink.display.protocol.readMessage
import dev.tablink.display.protocol.writeMessage
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Before
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.EOFException
import java.net.InetAddress
import java.net.Socket

class ChannelServerTest {
    private lateinit var server: ChannelServer

    @Before
    fun setUp() {
        server = ChannelServer(port = 0).also { it.start() }
    }

    @After
    fun tearDown() = server.close()

    private fun connect(channelByte: Int): Socket =
        Socket(InetAddress.getLoopbackAddress(), server.boundPort).also {
            it.getOutputStream().write(channelByte)
            it.getOutputStream().flush()
        }

    private fun await(ch: Channel): Socket = runBlocking { withTimeout(5000) { server.await(ch) } }

    @Test
    fun routesByChannelByte() {
        val video = connect(2)
        val control = connect(1)
        writeMessage(control.getOutputStream(), 0, Message.Ping)
        writeMessage(video.getOutputStream(), 0, Message.RequestKeyframe)

        assertEquals(Message.Ping, readMessage(await(Channel.CONTROL).getInputStream()).second)
        assertEquals(Message.RequestKeyframe, readMessage(await(Channel.VIDEO).getInputStream()).second)
    }

    @Test
    fun rejectsInvalidChannelByte() {
        val bad = connect(9)
        bad.soTimeout = 5000
        assertEquals(-1, bad.getInputStream().read())

        val good = connect(1)
        writeMessage(good.getOutputStream(), 0, Message.Ping)
        assertEquals(Message.Ping, readMessage(await(Channel.CONTROL).getInputStream()).second)
    }

    @Test
    fun reconnectReplacesOldSocket() {
        val first = connect(2)
        val firstServerSide = await(Channel.VIDEO)
        val second = connect(2)
        val secondServerSide = await(Channel.VIDEO)

        assertNotSame(firstServerSide, secondServerSide)
        first.soTimeout = 5000
        assertEquals(-1, first.getInputStream().read())
        writeMessage(second.getOutputStream(), 0, Message.Ping)
        assertEquals(Message.Ping, readMessage(secondServerSide.getInputStream()).second)
    }

    @Test
    fun awaitOrNullGivesUpWhenChannelNeverOpens() {
        connect(1)
        await(Channel.CONTROL)
        assertNull(runBlocking { server.awaitOrNull(Channel.VIDEO, 200) })
    }

    @Test
    fun awaitOrNullReturnsChannelThatOpensInTime() {
        val video = connect(2)
        writeMessage(video.getOutputStream(), 0, Message.Ping)
        val serverSide = runBlocking { server.awaitOrNull(Channel.VIDEO, 5000) }
        assertEquals(Message.Ping, readMessage(serverSide!!.getInputStream()).second)
    }

    @Test
    fun clientDisconnectMidMessageSurfacesEof() {
        val client = connect(2)
        val full = ByteArrayOutputStream().also {
            writeMessage(it, 0, Message.VideoFrame(true, byteArrayOf(1, 2, 3)))
        }.toByteArray()
        client.getOutputStream().write(full, 0, full.size - 1)
        client.getOutputStream().flush()
        val serverSide = await(Channel.VIDEO)
        client.close()

        assertThrows(EOFException::class.java) { readMessage(serverSide.getInputStream()) }
    }
}
