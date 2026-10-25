package ru.skreenshare.proektor

/**
 * Loss handling helpers (mirrors of proto/src/keyreq.rs and proto/src/audiorx.rs, plus audio concealment).
 * Pure Kotlin so they run in plain JVM tests.
 */

/**
 * When may the receiver ask the sender for a keyframe? A keyframe is hundreds of packets, so asking for one on every
 * loss (and every 150 ms again) feeds on itself on a lossy link. This asks at most once per cooldown (500 ms),
 * backs off exponentially (to 4 s) while requests stay unanswered, and never asks while a keyframe is arriving.
 */
class KeyPolicy(private val baseMs: Long = 500, private val maxMs: Long = 4000) {
    private var cooldownMs = baseMs
    private var lastSent = -1L
    private var hasSent = false
    private var need = false
    private var unanswered = false
    var requests = 0
        private set

    fun need() { need = true }
    fun needed() = need

    /** A keyframe was delivered to the decoder. */
    fun onKey() { need = false; unanswered = false; cooldownMs = baseMs }

    /** True if a KEYREQ should be sent now. [keyInFlight]: a keyframe is being received right now. */
    fun poll(nowMs: Long, keyInFlight: Boolean): Boolean {
        if (!need || keyInFlight) return false
        if (hasSent) {
            if (nowMs - lastSent < cooldownMs) return false
            if (unanswered) cooldownMs = minOf(cooldownMs * 2, maxMs)
        }
        unanswered = true
        hasSent = true
        lastSent = nowMs
        requests++
        return true
    }
}

/** Unpacks redundant audio: a single lost packet is rebuilt from the copy in the next one. */
class AudioRx(private val rate: Int, channels: Int) {
    class Chunk(val tsMs: Double, val data: ByteArray)

    private val channels = maxOf(channels, 1)
    private var haveLast = false
    private var lastSeq = 0
    var received = 0
        private set
    var recovered = 0
        private set
    var lost = 0
        private set

    fun reset() { haveLast = false }

    private fun chunkMs(bytes: Int) = (bytes / (2 * channels)).toDouble() * 1000.0 / rate

    /** Distance from the last accepted seq, or 0 if this packet is old / a duplicate. */
    private fun accept(seq: Int): Int {
        val d = if (haveLast) seq - lastSeq else 1
        if (d <= 0) return 0
        haveLast = true
        lastSeq = seq
        return d
    }

    fun onPlain(seq: Int, tsMs: Long, pcm: ByteArray, out: MutableList<Chunk>) {
        val d = accept(seq)
        if (d == 0) return
        received++
        lost += d - 1
        out.add(Chunk(tsMs.toDouble(), pcm))
    }

    fun onRed(seq: Int, tsMs: Long, prev: ByteArray?, cur: ByteArray, out: MutableList<Chunk>) {
        val d = accept(seq)
        if (d == 0) return
        received++
        var missing = d - 1
        if (d >= 2 && prev != null && prev.isNotEmpty()) {
            out.add(Chunk(tsMs.toDouble() - chunkMs(prev.size), prev))
            recovered++
            missing--
        }
        lost += missing
        out.add(Chunk(tsMs.toDouble(), cur))
    }
}

/**
 * Audio packet loss concealment. A hole of [frames] sample frames is not left as hard silence (that clicks): the
 * last few milliseconds before it are repeated, fading out linearly over at most [MAX_FADE_MS]; the rest of a long
 * hole is silence. The result always has exactly the hole's length, so the timeline (and lip-sync) is untouched.
 */
object Plc {
    const val LOOP_MS = 2.5
    const val MAX_FADE_MS = 12.0

    /** [tail]: the last audio written (16-bit interleaved PCM, may be empty). */
    fun conceal(tail: ByteArray?, frames: Int, channels: Int, rate: Int): ByteArray {
        val frameBytes = channels * 2
        val out = ByteArray(maxOf(frames, 0) * frameBytes)
        if (tail == null || frames <= 0) return out
        val loop = minOf(tail.size / frameBytes, maxOf(1, Math.round(LOOP_MS * rate / 1000.0).toInt()))
        if (loop < 8) return out
        val fadeFrames = minOf(frames, Math.round(MAX_FADE_MS * rate / 1000.0).toInt())
        val srcStart = tail.size - loop * frameBytes
        for (f in 0 until fadeFrames) {
            val gain = 1.0 - (f + 1).toDouble() / (fadeFrames + 1)
            val src = srcStart + (f % loop) * frameBytes
            for (c in 0 until channels) {
                val o = src + c * 2
                val v = ((tail[o].toInt() and 0xFF) or (tail[o + 1].toInt() shl 8)).toShort().toInt()
                val s = Math.round(v * gain).toInt().coerceIn(-32768, 32767)
                val d = f * frameBytes + c * 2
                out[d] = s.toByte()
                out[d + 1] = (s shr 8).toByte()
            }
        }
        return out
    }
}
