package dev.tablink.display.ui

import android.media.MediaCodecInfo.CodecProfileLevel
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Bundle
import android.util.Log
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.WindowManager
import android.widget.FrameLayout
import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.ComposeView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import dev.tablink.display.net.ChannelServer
import dev.tablink.display.protocol.Channel
import dev.tablink.display.protocol.Message
import dev.tablink.display.protocol.readMessage
import dev.tablink.display.protocol.writeMessage
import dev.tablink.display.ui.theme.TabLinkTheme
import dev.tablink.display.video.DecoderCaps
import dev.tablink.display.video.DisplayInfo
import dev.tablink.display.video.PanelMode
import dev.tablink.display.video.VideoDecoder
import dev.tablink.display.video.buildHello
import dev.tablink.display.video.displayInfoOf
import kotlinx.coroutines.channels.ClosedReceiveChannelException
import kotlinx.coroutines.runBlocking
import java.io.IOException
import java.io.OutputStream
import java.net.Socket
import java.util.concurrent.Executors
import kotlin.concurrent.thread

/**
 * The second screen. Listens for the host, shows the stream full screen and
 * falls back to the waiting screen whenever the link drops.
 */
class StreamActivity : ComponentActivity(), SurfaceHolder.Callback {
    private val server = ChannelServer()
    private val decoder = VideoDecoder(::requestKeyframe, ::acknowledgeFrame)

    /** Socket writes must stay off the main thread and be serialized. */
    private val controlWriter = Executors.newSingleThreadExecutor()

    private val lock = Any()
    private var surface: Surface? = null
    private var controlOut: OutputStream? = null
    private var config: Message.Config? = null
    private var parameterSets: ByteArray? = null

    private var streaming by mutableStateOf(false)
    @Volatile private var destroyed = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        WindowCompat.setDecorFitsSystemWindows(window, false)

        val surfaceView = SurfaceView(this).also { it.holder.addCallback(this) }
        val overlay = ComposeView(this).apply {
            setContent { TabLinkTheme { if (!streaming) WaitingScreen() } }
        }
        setContentView(FrameLayout(this).apply {
            addView(surfaceView, MATCH_PARENT, MATCH_PARENT)
            addView(overlay, MATCH_PARENT, MATCH_PARENT)
        })

        try {
            server.start()
        } catch (e: IOException) {
            // Another instance already owns the port; nothing to listen on.
            Log.e(TAG, "cannot listen", e)
            finish()
            return
        }
        thread(name = "tablink-session", isDaemon = true) { sessionLoop() }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    private fun hideSystemBars() {
        WindowInsetsControllerCompat(window, window.decorView).apply {
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            hide(WindowInsetsCompat.Type.systemBars())
        }
    }

    override fun onDestroy() {
        destroyed = true
        server.close()
        controlWriter.shutdownNow()
        decoder.close()
        super.onDestroy()
    }

    // --- Surface lifecycle -------------------------------------------------

    override fun surfaceCreated(holder: SurfaceHolder) {
        synchronized(lock) { surface = holder.surface }
        // Coming back from the background: restart decoding from a keyframe.
        if (startDecoderIfReady()) requestKeyframe()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        synchronized(lock) { surface = null }
        decoder.release()
    }

    /** Configures the decoder once a surface, a config and parameter sets all exist. */
    private fun startDecoderIfReady(): Boolean {
        val (target, cfg, csd) = synchronized(lock) {
            Triple(surface ?: return false, config ?: return false, parameterSets ?: return false)
        }
        target.setFrameRate(cfg.fps.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        decoder.configure(target, cfg.width, cfg.height, cfg.fps, csd)
        return true
    }

    // --- Session -----------------------------------------------------------

    private fun sessionLoop() {
        while (!destroyed) {
            var control: Socket? = null
            var video: Socket? = null
            try {
                val controlSocket = runBlocking { server.await(Channel.CONTROL) }
                control = controlSocket
                val out = controlSocket.getOutputStream()
                writeMessage(out, 0, buildHello(displayInfo(), decoderCaps()))

                val cfg = generateSequence { readMessage(controlSocket.getInputStream()).second }
                    .filterIsInstance<Message.Config>().first()
                synchronized(lock) {
                    controlOut = out
                    config = cfg
                    parameterSets = null
                }
                runOnUiThread { applyRefreshRate(cfg.fps) }
                thread(name = "tablink-control", isDaemon = true) { controlLoop(controlSocket, out) }

                // The host opens video right after sending Config. If it does
                // not, it is gone; go back to listening for a new session.
                val videoSocket = runBlocking { server.awaitOrNull(Channel.VIDEO, VIDEO_CHANNEL_TIMEOUT_MS) }
                    ?: throw IOException("host did not open the video channel")
                video = videoSocket
                videoLoop(videoSocket)
            } catch (e: IOException) {
                Log.i(TAG, "session ended: ${e.message}")
            } catch (e: ClosedReceiveChannelException) {
                return // the activity is going away
            } catch (e: InterruptedException) {
                return
            } finally {
                synchronized(lock) {
                    controlOut = null
                    config = null
                    parameterSets = null
                }
                decoder.release()
                control?.closeQuietly()
                video?.closeQuietly()
                streaming = false
            }
        }
    }

    private fun videoLoop(video: Socket) {
        val input = video.getInputStream().buffered(SOCKET_BUFFER_BYTES)
        while (true) {
            val (ptsUs, msg) = readMessage(input)
            when (msg) {
                is Message.VideoConfig -> {
                    synchronized(lock) { parameterSets = msg.data }
                    startDecoderIfReady()
                }
                is Message.VideoFrame -> {
                    decoder.submit(msg.data, ptsUs, msg.keyframe)
                    if (!streaming) streaming = true
                }
                else -> Unit
            }
        }
    }

    /** Answers the host's pings; ends when the control socket closes. */
    private fun controlLoop(control: Socket, out: OutputStream) {
        try {
            while (true) {
                if (readMessage(control.getInputStream()).second == Message.Ping) send(out, Message.Pong)
            }
        } catch (e: IOException) {
            // The session loop notices the drop through the video socket; make
            // sure it does even if only the control channel failed.
            control.closeQuietly()
            server.closeChannel(Channel.VIDEO)
        }
    }

    private fun requestKeyframe() {
        val out = synchronized(lock) { controlOut } ?: return
        send(out, Message.RequestKeyframe)
    }

    /** Tells the host a frame was rendered, so it can measure latency. */
    private fun acknowledgeFrame(ptsUs: Long) {
        val out = synchronized(lock) { controlOut } ?: return
        send(out, Message.FrameAck, ptsUs)
    }

    private fun send(out: OutputStream, msg: Message, ptsUs: Long = 0) {
        if (controlWriter.isShutdown) return
        controlWriter.execute {
            try {
                writeMessage(out, ptsUs, msg)
            } catch (e: IOException) {
                Log.i(TAG, "control write failed: ${e.message}")
            }
        }
    }

    // --- Device capabilities ------------------------------------------------

    private fun displayInfo(): DisplayInfo {
        val modes = display?.supportedModes.orEmpty().map { PanelMode(it.physicalWidth, it.physicalHeight, it.refreshRate) }
        return displayInfoOf(modes, resources.displayMetrics.densityDpi)
    }

    /** Asks the panel for the mode whose refresh rate matches the stream. */
    private fun applyRefreshRate(fps: Int) {
        val current = display?.mode ?: return
        val match = display?.supportedModes.orEmpty()
            .filter { it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight }
            .minByOrNull { kotlin.math.abs(it.refreshRate - fps) } ?: return
        window.attributes = window.attributes.also { it.preferredDisplayModeId = match.modeId }
    }

    private fun decoderCaps(): Int {
        var caps = 0
        for (info in MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos) {
            if (info.isEncoder || MediaFormat.MIMETYPE_VIDEO_HEVC !in info.supportedTypes) continue
            for (pl in info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_HEVC).profileLevels) {
                when (pl.profile) {
                    CodecProfileLevel.HEVCProfileMain -> caps = caps or DecoderCaps.HEVC_MAIN
                    CodecProfileLevel.HEVCProfileMain10 -> caps = caps or DecoderCaps.HEVC_MAIN10
                    CodecProfileLevel.HEVCProfileMain10HDR10,
                    CodecProfileLevel.HEVCProfileMain10HDR10Plus ->
                        caps = caps or DecoderCaps.HEVC_MAIN10 or DecoderCaps.HDR10
                }
            }
        }
        return caps
    }

    private fun Socket.closeQuietly() {
        try {
            close()
        } catch (_: IOException) {
        }
    }

    private companion object {
        const val TAG = "TabLinkStream"
        const val SOCKET_BUFFER_BYTES = 256 * 1024
        const val VIDEO_CHANNEL_TIMEOUT_MS = 5000L
    }
}
