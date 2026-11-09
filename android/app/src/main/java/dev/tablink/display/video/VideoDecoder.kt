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
class VideoDecoder(private val onNeedKeyframe: () -> Unit) {
    private class Frame(val data: ByteArray, val ptsUs: Long, val keyframe: Boolean)

    private val lock = Any()
    private val thread = HandlerThread("tablink-decoder").apply { start() }
    private val handler = Handler(thread.looper)

    private var codec: MediaCodec? = null
    private val pending = ArrayDeque<Frame>()
    private val freeInputs = ArrayDeque<Int>()
    private var waitingForKeyframe = true

    fun configure(surface: Surface, width: Int, height: Int, fps: Int, csd: ByteArray): Unit = synchronized(lock) {
        releaseLocked()
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_HEVC, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, MAX_INPUT_BYTES)
        }
        try {
            val created = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_HEVC)
            created.setCallback(Callback(created), handler)
            created.configure(format, surface, null, 0)
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
                drainLocked()
                false
            }
        }
        if (askForKeyframe) onNeedKeyframe()
    }

    fun release(): Unit = synchronized(lock) { releaseLocked() }

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
                }
            }
        }

        override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
            Log.e(TAG, "decoder error", e)
            synchronized(lock) {
                if (!current()) return
                // Flushing would need a restart in async mode; dropping queued
                // input and resyncing on a keyframe is enough for transient errors.
                pending.clear()
                waitingForKeyframe = true
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
