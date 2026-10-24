package ru.skreenshare.proektor

import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.os.Build
import android.util.Log
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * 16-bit PCM playback with a small jitter buffer: wait for ~40 ms before starting, re-buffer after an
 * underrun, and drop the oldest audio if the queue grows past ~150 ms (the two clocks never agree exactly).
 */
class AudioPlayer(private val rate: Int, private val channels: Int) {
    private val track: AudioTrack
    private val queue = LinkedBlockingQueue<ByteArray>()
    private val queued = AtomicInteger()
    @Volatile private var running = true
    private val thread: Thread
    private val bytesPerMs = rate * channels * 2 / 1000

    init {
        val chMask = if (channels == 1) AudioFormat.CHANNEL_OUT_MONO else AudioFormat.CHANNEL_OUT_STEREO
        val minBuf = AudioTrack.getMinBufferSize(rate, chMask, AudioFormat.ENCODING_PCM_16BIT)
        val bufSize = maxOf(minBuf, bytesPerMs * 80)
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

    fun push(pcm: ByteArray) {
        queue.offer(pcm)
        queued.addAndGet(pcm.size)
    }

    private fun loop() {
        var buffering = true
        try {
            while (running) {
                if (buffering) {
                    if (queued.get() >= bytesPerMs * START_MS) buffering = false
                    else { Thread.sleep(5); continue }
                }
                // Too far behind (clock drift, burst): throw away the oldest audio.
                if (queued.get() > bytesPerMs * MAX_MS) {
                    while (queued.get() > bytesPerMs * START_MS) {
                        val d = queue.poll() ?: break
                        queued.addAndGet(-d.size)
                    }
                }
                val chunk = queue.poll(30, TimeUnit.MILLISECONDS)
                if (chunk == null) { buffering = true; continue }
                queued.addAndGet(-chunk.size)
                track.write(chunk, 0, chunk.size) // blocks when the track's buffer is full: this paces us
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
