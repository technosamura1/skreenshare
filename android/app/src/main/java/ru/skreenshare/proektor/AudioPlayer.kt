package ru.skreenshare.proektor

import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.os.Build
import android.util.Log
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * 16-bit PCM playback that doubles as the master clock for lip-sync.
 *
 * Chunks carry the sender's capture time. Holes (lost packets, PC silent) are filled with silence so that
 * "frames written" stays a faithful map of media time ([AudioTimeline]); [clockMs] turns the AudioTrack's real
 * playback position (what the speaker is playing now) into sender media time.
 * Small jitter buffer: wait for ~40 ms before starting, re-buffer after an underrun, drop the oldest audio
 * if the queue grows past ~150 ms.
 */
class AudioPlayer(private val rate: Int, private val channels: Int) {
    private class Item(val tsMs: Double, val data: ByteArray?, val delayMs: Int)

    private val track: AudioTrack
    private val queue = LinkedBlockingQueue<Item>()
    private val queued = AtomicInteger()
    @Volatile private var running = true
    private val thread: Thread
    private val bytesPerMs = rate * channels * 2 / 1000
    private val frameBytes = channels * 2
    private val timeline = AudioTimeline(rate)
    /** Last ~5 ms that went to the speaker: the material for concealing a hole (see [Plc]). */
    private var tail: ByteArray? = null

    @Volatile private var lastWriteNanos = 0L
    private val clockLock = Any()
    private var anchorPos = -1L
    private var anchorNano = 0L
    private val ts = AudioTimestamp()

    init {
        val chMask = if (channels == 1) AudioFormat.CHANNEL_OUT_MONO else AudioFormat.CHANNEL_OUT_STEREO
        val minBuf = AudioTrack.getMinBufferSize(rate, chMask, AudioFormat.ENCODING_PCM_16BIT)
        val bufSize = maxOf(minBuf, bytesPerMs * 60)
        track = if (Build.VERSION.SDK_INT >= 23) {
            val b = AudioTrack.Builder()
                .setAudioFormat(
                    AudioFormat.Builder().setSampleRate(rate).setChannelMask(chMask)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT).build()
                )
                .setBufferSizeInBytes(bufSize)
                .setTransferMode(AudioTrack.MODE_STREAM)
            if (Build.VERSION.SDK_INT >= 26) b.setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            b.build()
        } else {
            @Suppress("DEPRECATION")
            AudioTrack(AudioManager.STREAM_MUSIC, rate, chMask, AudioFormat.ENCODING_PCM_16BIT, bufSize, AudioTrack.MODE_STREAM)
        }
        track.play()
        thread = Thread({ loop() }, "audio-play").also { it.start() }
    }

    /** [tsMs]: sender capture time of the first sample in [pcm]. */
    fun push(tsMs: Long, pcm: ByteArray) {
        queue.offer(Item(tsMs.toDouble(), pcm, 0))
        queued.addAndGet(pcm.size)
    }

    /** Plays everything after this point [ms] later (keeps lip-sync when the picture can't keep up). */
    fun delayBy(ms: Int) {
        if (ms > 0) queue.offer(Item(0.0, null, ms))
    }

    /** Sender media time that is coming out of the speaker right now, or null if audio isn't running. */
    fun clockMs(): Double? {
        val now = System.nanoTime()
        val lw = lastWriteNanos
        if (lw == 0L || now - lw > 1_000_000_000L) return null
        synchronized(clockLock) {
            try {
                if (track.getTimestamp(ts)) {
                    if (ts.framePosition != anchorPos) { anchorPos = ts.framePosition; anchorNano = ts.nanoTime }
                } else {
                    val p = track.playbackHeadPosition.toLong() and 0xFFFFFFFFL
                    if (p != anchorPos) { anchorPos = p; anchorNano = now }
                }
            } catch (_: Throwable) { return null }
            if (anchorPos < 0) return null
            val elapsedMs = (now - anchorNano) / 1e6
            if (elapsedMs > 1500) return null
            val base = timeline.mediaAt(anchorPos) ?: return null
            return base + elapsedMs
        }
    }

    private fun writeAll(b: ByteArray, off: Int, len: Int) {
        var o = off
        var left = len
        while (left > 0 && running) {
            val n = track.write(b, o, left)
            if (n <= 0) { Thread.sleep(2); continue }
            o += n; left -= n
        }
        lastWriteNanos = System.nanoTime()
    }

    /** Fills a hole of [frames] sample frames with a faded repeat of the last audio instead of a hard cut to silence. */
    private fun writeConceal(frames: Int) {
        val c = Plc.conceal(tail, frames, channels, rate)
        writeAll(c, 0, c.size)
        tail = null // what follows is a fresh start
    }

    private fun writeSilence(frames: Int) {
        var left = frames
        val z = ByteArray(minOf(frames, 4800) * frameBytes)
        while (left > 0 && running) {
            val n = minOf(left, 4800)
            writeAll(z, 0, n * frameBytes)
            left -= n
        }
    }

    private fun loop() {
        var buffering = true
        var resync = true
        try {
            while (running) {
                if (buffering) {
                    if (queued.get() >= bytesPerMs * START_MS) buffering = false
                    else { Thread.sleep(5); continue }
                }
                // Too far behind (clock drift, burst): throw away the oldest audio and start a new time segment.
                if (queued.get() > bytesPerMs * MAX_MS) {
                    while (queued.get() > bytesPerMs * START_MS) {
                        val d = queue.poll() ?: break
                        d.data?.let { queued.addAndGet(-it.size) }
                    }
                    resync = true
                }
                val item = queue.poll(30, TimeUnit.MILLISECONDS)
                if (item == null) { buffering = true; resync = true; continue }
                val data = item.data
                if (data == null) {
                    writeSilence(timeline.insertDelay(item.delayMs.toDouble()))
                    continue
                }
                queued.addAndGet(-data.size)
                val frames = data.size / frameBytes
                val plan = timeline.plan(item.tsMs, frames, resync)
                resync = false
                if (plan.drop) continue
                if (plan.silenceFrames > 0) writeConceal(plan.silenceFrames)
                val skip = plan.skipFrames * frameBytes
                writeAll(data, skip, frames * frameBytes - skip) // blocks when the track's buffer is full: this paces us
                val end = frames * frameBytes
                tail = data.copyOfRange(maxOf(skip, end - bytesPerMs * 5 / frameBytes * frameBytes), end)
            }
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "audio: $e")
        }
    }

    fun release() {
        running = false
        try { thread.join(300) } catch (_: Throwable) {}
        try { track.pause(); track.flush() } catch (_: Throwable) {}
        try { track.release() } catch (_: Throwable) {}
    }

    companion object {
        private const val TAG = "SkpAudio"
        private const val START_MS = 40
        private const val MAX_MS = 150
    }
}
