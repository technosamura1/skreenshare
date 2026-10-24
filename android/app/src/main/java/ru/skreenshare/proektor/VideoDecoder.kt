package ru.skreenshare.proektor

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * H.264 -> Surface through MediaCodec, tuned for latency: no timestamps-based scheduling (every decoded frame
 * is shown as soon as it comes out), a tiny input queue, and "drop and ask for a keyframe" when behind.
 */
class VideoDecoder(
    surface: Surface,
    width: Int,
    height: Int,
    fps: Int,
    /** Called (from the decoder thread) when frames were dropped and a keyframe is needed. */
    private val onNeedKey: () -> Unit,
) {
    private class Item(val data: ByteArray, val tsUs: Long)

    private val codec: MediaCodec
    private val queue = ArrayBlockingQueue<Item>(3)
    @Volatile private var running = true
    val decoded = AtomicInteger()
    val dropped = AtomicInteger()
    private val feeder: Thread
    private val drainer: Thread

    init {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
        format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 1 shl 20)
        if (Build.VERSION.SDK_INT >= 23) {
            format.setInteger(MediaFormat.KEY_PRIORITY, 0) // real-time
            format.setInteger(MediaFormat.KEY_OPERATING_RATE, maxOf(fps, 30))
        }
        if (Build.VERSION.SDK_INT >= 30) format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        // Vendor switches for low-latency decoding; decoders that don't know them ignore them.
        for (k in listOf("vendor.rtc-ext-dec-low-latency.enable", "vendor.low-latency.enable")) {
            try { format.setInteger(k, 1) } catch (_: Throwable) {}
        }
        codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        try {
            codec.configure(format, surface, null, 0)
            codec.start()
        } catch (e: Throwable) {
            try { codec.release() } catch (_: Throwable) {}
            throw e
        }
        Log.i(TAG, "decoder ${codec.name} ${width}x$height@$fps")
        feeder = Thread({ feedLoop() }, "video-feed").also { it.start() }
        drainer = Thread({ drainLoop() }, "video-drain").also { it.start() }
    }

    /** Queue one access unit. If the decoder is behind, everything queued is dropped and a keyframe requested. */
    fun submit(data: ByteArray, tsMs: Int): Boolean {
        if (!queue.offer(Item(data, tsMs.toLong() * 1000))) {
            dropped.addAndGet(queue.size + 1)
            queue.clear()
            return false
        }
        return true
    }

    private fun feedLoop() {
        try {
            while (running) {
                val item = queue.poll(50, TimeUnit.MILLISECONDS) ?: continue
                var idx = codec.dequeueInputBuffer(20_000)
                if (idx < 0) {
                    // Decoder isn't taking input: skip this frame (and the chain depending on it).
                    dropped.incrementAndGet()
                    queue.clear()
                    onNeedKey()
                    continue
                }
                val buf = codec.getInputBuffer(idx)!!
                buf.clear()
                buf.put(item.data)
                codec.queueInputBuffer(idx, 0, item.data.size, item.tsUs, 0)
            }
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "feed: $e")
        }
    }

    private fun drainLoop() {
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                val idx = codec.dequeueOutputBuffer(info, 10_000)
                if (idx >= 0) {
                    decoded.incrementAndGet()
                    codec.releaseOutputBuffer(idx, true) // render right now
                }
            }
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "drain: $e")
        }
    }

    fun release() {
        running = false
        try { feeder.join(300) } catch (_: Throwable) {}
        try { drainer.join(300) } catch (_: Throwable) {}
        try { codec.stop() } catch (_: Throwable) {}
        try { codec.release() } catch (_: Throwable) {}
    }

    companion object { private const val TAG = "SkpVideo" }
}
