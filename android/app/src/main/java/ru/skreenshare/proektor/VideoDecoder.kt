package ru.skreenshare.proektor

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Process
import android.util.Log
import android.view.Surface
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * H.264 -> Surface through MediaCodec, tuned for latency: a tiny input queue and "drop and ask for a keyframe"
 * when the decoder is behind. Decoded frames are shown against [clock] (lip-sync): held until the sound with the
 * same timestamp is playing, dropped if more than ~100 ms late. Without a clock ([paced] = false) every decoded
 * frame is shown at once.
 */
class VideoDecoder(
    surface: Surface,
    width: Int,
    height: Int,
    fps: Int,
    /** Called (from the decoder thread) when frames were dropped and a keyframe is needed. */
    private val onNeedKey: () -> Unit,
    private val clock: MediaClock? = null,
    private val sync: SyncController? = null,
) {
    private class Item(val data: ByteArray, val tsUs: Long)

    private val codec: MediaCodec
    private val queue = ArrayBlockingQueue<Item>(5)
    @Volatile private var running = true
    val decoded = AtomicInteger()
    val dropped = AtomicInteger()
    /** Frames actually put on the screen. */
    val shown = AtomicInteger()
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
        if (!queue.offer(Item(data, (tsMs.toLong() and 0xFFFFFFFFL) * 1000))) {
            dropped.addAndGet(queue.size + 1)
            queue.clear()
            return false
        }
        return true
    }

    private fun feedLoop() {
        try { Process.setThreadPriority(Process.THREAD_PRIORITY_DISPLAY) } catch (_: Throwable) {}
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
                val buf = inputBuffer(idx)
                buf.clear()
                buf.put(item.data)
                codec.queueInputBuffer(idx, 0, item.data.size, item.tsUs, 0)
            }
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "feed: $e")
        }
    }

    @Suppress("DEPRECATION")
    private fun inputBuffer(idx: Int) =
        if (Build.VERSION.SDK_INT >= 21) codec.getInputBuffer(idx)!! else codec.inputBuffers[idx]

    private fun drainLoop() {
        try { Process.setThreadPriority(Process.THREAD_PRIORITY_DISPLAY) } catch (_: Throwable) {}
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                val idx = codec.dequeueOutputBuffer(info, 10_000)
                if (idx >= 0) {
                    decoded.incrementAndGet()
                    present(idx, info.presentationTimeUs / 1000.0)
                }
            }
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "drain: $e")
        }
    }

    private var consecutiveDrops = 0

    /** Shows (or drops) one decoded frame at the moment the playback clock reaches its timestamp. */
    private fun present(idx: Int, ptsMs: Double) {
        val c = clock
        var now = System.nanoTime() / 1_000_000
        var clockMs = c?.nowMs(now)
        if (c == null || clockMs == null) {
            codec.releaseOutputBuffer(idx, true)
            shown.incrementAndGet()
            return
        }
        val d = VideoPacer.decide(ptsMs, clockMs, consecutiveDrops)
        when (d.kind) {
            VideoPacer.Kind.DROP -> {
                dropped.incrementAndGet()
                consecutiveDrops++
                codec.releaseOutputBuffer(idx, false)
            }
            else -> {
                if (d.kind == VideoPacer.Kind.WAIT) {
                    val deadline = now + d.waitMs
                    while (running) {
                        now = System.nanoTime() / 1_000_000
                        clockMs = c.nowMs(now) ?: break
                        // Wait until the clock reaches the frame (it can jump when audio is re-synced), never more than the cap.
                        if (ptsMs - clockMs <= 1.0 || now >= deadline) break
                        Thread.sleep(minOf(4L, maxOf(1L, (ptsMs - clockMs).toLong())))
                    }
                }
                consecutiveDrops = 0
                codec.releaseOutputBuffer(idx, true)
                shown.incrementAndGet()
            }
        }
        val late = (clockMs ?: ptsMs) - ptsMs
        sync?.onFrame(late, System.nanoTime() / 1_000_000)
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
