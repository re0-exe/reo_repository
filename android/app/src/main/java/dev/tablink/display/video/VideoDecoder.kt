package dev.tablink.display.video

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer

/**
 * Low-latency HEVC decode straight to a [Surface].
 *
 * Frames are rendered the moment they are decoded. Anything that leaves the
 * decoder without a valid reference chain (an error, a fresh surface, a
 * backlog that had to be dropped) ends in [onNeedKeyframe], and frames are
 * discarded until that keyframe arrives.
 */
class VideoDecoder(
    private val onNeedKeyframe: () -> Unit,
    /** Called with a frame's timestamp once it has been handed to the display. */
    private val onFrameRendered: (ptsUs: Long) -> Unit = {},
) {
    private class Frame(val data: ByteArray, val ptsUs: Long, val keyframe: Boolean)
    private class Setup(val surface: Surface, val width: Int, val height: Int, val fps: Int, val csd: ByteArray)

    private val lock = Any()
    private val thread = HandlerThread("tablink-decoder").apply { start() }
    private val handler = Handler(thread.looper)

    private var codec: MediaCodec? = null
    private val pending = ArrayDeque<Frame>()
    private val freeInputs = ArrayDeque<Int>()
    private var waitingForKeyframe = true
    /** What the decoder was last configured with, to rebuild it after a fatal error. */
    private var setup: Setup? = null

    fun configure(surface: Surface, width: Int, height: Int, fps: Int, csd: ByteArray): Unit = synchronized(lock) {
        configureLocked(Setup(surface, width, height, fps, csd))
    }

    private fun configureLocked(s: Setup) {
        releaseLocked()
        setup = s
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_HEVC, s.width, s.height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(s.csd))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            setInteger(MediaFormat.KEY_FRAME_RATE, s.fps)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, MAX_INPUT_BYTES)
        }
        try {
            val created = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_HEVC)
            created.setCallback(Callback(created), handler)
            created.configure(format, s.surface, null, 0)
            created.start()
            codec = created
        } catch (e: Exception) {
            // IllegalStateException / CodecException / IOException: the
            // decoder could not be brought up on this surface.
            Log.e(TAG, "decoder configure failed", e)
            releaseLocked()
        }
        waitingForKeyframe = true
    }

    fun submit(data: ByteArray, ptsUs: Long, keyframe: Boolean) {
        val askForKeyframe = synchronized(lock) {
            if (codec == null) return
            if (waitingForKeyframe && !keyframe) return
            if (keyframe) waitingForKeyframe = false
            if (pending.size >= MAX_PENDING_FRAMES) {
                // The decoder is not keeping up. Dropping a frame breaks the
                // reference chain, so drop everything and start from a keyframe.
                pending.clear()
                waitingForKeyframe = true
                true
            } else {
                pending.addLast(Frame(data, ptsUs, keyframe))
                try {
                    drainLocked()
                    false
                } catch (e: IllegalStateException) {
                    // The codec is in an error state (CodecException is an
                    // IllegalStateException). Rebuild it and start over.
                    Log.w(TAG, "queueing input failed; restarting decoder", e)
                    restartLocked()
                    true
                }
            }
        }
        if (askForKeyframe) onNeedKeyframe()
    }

    fun release(): Unit = synchronized(lock) {
        releaseLocked()
        setup = null
    }

    /** Rebuilds the decoder from its last configuration, if the surface is still there. */
    private fun restartLocked() {
        val s = setup
        releaseLocked()
        if (s != null && s.surface.isValid) configureLocked(s)
    }

    /** Releases the decoder and its thread; the instance is unusable afterwards. */
    fun close() {
        release()
        thread.quitSafely()
    }

    private fun releaseLocked() {
        codec?.let {
            try {
                it.stop()
            } catch (_: IllegalStateException) {
            }
            it.release()
        }
        codec = null
        pending.clear()
        freeInputs.clear()
        waitingForKeyframe = true
    }

    private fun drainLocked() {
        val active = codec ?: return
        while (pending.isNotEmpty() && freeInputs.isNotEmpty()) {
            val frame = pending.removeFirst()
            val index = freeInputs.removeFirst()
            val buffer = active.getInputBuffer(index) ?: continue
            if (frame.data.size > buffer.capacity()) {
                Log.w(TAG, "frame of ${frame.data.size} bytes exceeds the input buffer; dropped")
                freeInputs.addFirst(index)
                waitingForKeyframe = true
                handler.post { onNeedKeyframe() }
                continue
            }
            buffer.clear()
            buffer.put(frame.data)
            val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
            active.queueInputBuffer(index, 0, frame.data.size, frame.ptsUs, flags)
        }
    }

    private inner class Callback(private val owner: MediaCodec) : MediaCodec.Callback() {
        // Callbacks of a codec that has since been replaced are ignored.
        private fun current() = codec === owner

        override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
            synchronized(lock) {
                if (!current()) return
                freeInputs.addLast(index)
                try {
                    drainLocked()
                } catch (e: IllegalStateException) {
                    Log.w(TAG, "queueing input failed", e)
                }
            }
        }

        override fun onOutputBufferAvailable(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
            synchronized(lock) {
                if (!current()) return
                try {
                    c.releaseOutputBuffer(index, true)
                } catch (e: IllegalStateException) {
                    Log.w(TAG, "render failed", e)
                    return
                }
            }
            onFrameRendered(info.presentationTimeUs)
        }

        override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
            Log.e(TAG, "decoder error", e)
            synchronized(lock) {
                if (!current()) return
                if (e.isRecoverable || e.isTransient) {
                    // Dropping queued input and resyncing on a keyframe is enough.
                    pending.clear()
                    waitingForKeyframe = true
                } else {
                    restartLocked()
                }
            }
            onNeedKeyframe()
        }

        override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) = Unit
    }

    private companion object {
        const val TAG = "TabLinkDecoder"
        /** Roughly a quarter second at 120 fps. */
        const val MAX_PENDING_FRAMES = 30
        /** Generous ceiling for one keyframe at the highest supported bitrate. */
        const val MAX_INPUT_BYTES = 8 * 1024 * 1024
    }
}
