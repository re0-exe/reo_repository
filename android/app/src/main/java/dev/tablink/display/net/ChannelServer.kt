package dev.tablink.display.net

import dev.tablink.display.protocol.Channel
import dev.tablink.display.protocol.Protocol
import java.io.Closeable
import java.io.IOException
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import kotlin.concurrent.thread
import kotlinx.coroutines.channels.Channel as Queue

/**
 * Listens on loopback for the host's connections (reached through
 * `adb forward`) and hands each one out by the channel id in its first byte.
 *
 * A connection with an invalid channel byte is closed without affecting the
 * others. When a channel connects again, the previous socket for that channel
 * is closed and the new one takes its place, which is how a reconnect after a
 * cable pull is absorbed.
 */
class ChannelServer(private val port: Int = Protocol.PORT) : Closeable {
    private val server = ServerSocket()
    private val queues = Channel.entries.associateWith { Queue<Socket>(Queue.CONFLATED) }
    private val current = HashMap<Channel, Socket>()

    val boundPort: Int get() = server.localPort

    fun start() {
        server.reuseAddress = true
        server.bind(InetSocketAddress(InetAddress.getLoopbackAddress(), port))
        thread(name = "tablink-accept", isDaemon = true) { acceptLoop() }
    }

    /** Suspends until the host opens (or re-opens) [ch]. */
    suspend fun await(ch: Channel): Socket = queues.getValue(ch).receive()

    private fun acceptLoop() {
        while (!server.isClosed) {
            val socket = try {
                server.accept()
            } catch (e: IOException) {
                return // server closed
            }
            // The channel byte arrives with the connection; a short timeout
            // keeps a silent peer from stalling later connections.
            thread(name = "tablink-handshake", isDaemon = true) { handshake(socket) }
        }
    }

    private fun handshake(socket: Socket) {
        val channel = try {
            socket.soTimeout = HANDSHAKE_TIMEOUT_MS
            Channel.fromId(socket.getInputStream().read())
        } catch (e: IOException) {
            null
        }
        if (channel == null) {
            socket.closeQuietly()
            return
        }
        socket.soTimeout = 0
        socket.tcpNoDelay = true
        synchronized(current) { current.put(channel, socket) }?.closeQuietly()
        queues.getValue(channel).trySend(socket)
    }

    /** Drops the current connection of [ch], waking whoever is reading from it. */
    fun closeChannel(ch: Channel) {
        synchronized(current) { current.remove(ch) }?.closeQuietly()
    }

    override fun close() {
        server.closeQuietly()
        // Wakes anything suspended in [await].
        queues.values.forEach { it.close() }
        synchronized(current) {
            current.values.forEach { it.closeQuietly() }
            current.clear()
        }
    }

    private fun Closeable.closeQuietly() {
        try {
            close()
        } catch (_: IOException) {
        }
    }

    private companion object {
        const val HANDSHAKE_TIMEOUT_MS = 3000
    }
}
