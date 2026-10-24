package ru.skreenshare.proektor

/*
 * Lip-sync logic without any Android classes (unit-tested on the JVM).
 *
 * All timestamps are "sender media time" in ms: the PC stamps video frames and audio chunks with the same clock
 * (ms since the stream started). On the projector the audio output is the master clock: the picture of a frame
 * stamped T is shown when the sound stamped T comes out of the speaker.
 */

/** Maps the frames written to the AudioTrack to sender media time. Used from the audio writer and the video thread. */
class AudioTimeline(private val rate: Int) {
    class Plan(val silenceFrames: Int, val skipFrames: Int, val drop: Boolean)

    private class Seg(val startFrame: Long, val baseMs: Double)

    private val segs = ArrayList<Seg>()
    private var written = 0L

    private fun msToFrames(ms: Double) = Math.round(ms * rate / 1000.0)

    private fun mediaAtLocked(frame: Long): Double {
        var s = segs[0]
        for (i in segs.indices.reversed()) if (segs[i].startFrame <= frame) { s = segs[i]; break }
        return s.baseMs + (frame - s.startFrame) * 1000.0 / rate
    }

    private fun newSeg(tsMs: Double) {
        segs.add(Seg(written, tsMs))
        if (segs.size > 64) segs.removeAt(0)
    }

    /**
     * A chunk stamped [tsMs] with [frames] sample frames is about to be written. Returns what to do first:
     * write [Plan.silenceFrames] of silence (a hole in the stream), skip [Plan.skipFrames] from the chunk's head
     * (it overlaps what is already written) or drop it. Keeps the frames<->time mapping within [TOL_MS] of the stamps.
     * [resync] starts a new time segment without filling anything (first chunk, after an underrun or after dropping audio).
     */
    @Synchronized
    fun plan(tsMs: Double, frames: Int, resync: Boolean): Plan {
        if (segs.isEmpty() || resync) {
            newSeg(tsMs); written += frames
            return Plan(0, 0, false)
        }
        val gap = tsMs - mediaAtLocked(written)
        if (gap > MAX_GAP_MS || gap < -MAX_GAP_MS) {
            newSeg(tsMs); written += frames
            return Plan(0, 0, false)
        }
        if (gap > TOL_MS) {
            val s = msToFrames(gap).toInt()
            written += s + frames
            return Plan(s, 0, false)
        }
        if (gap < -TOL_MS) {
            val o = msToFrames(-gap).toInt()
            if (o >= frames) return Plan(0, 0, true)
            written += frames - o
            return Plan(0, o, false)
        }
        written += frames
        return Plan(0, 0, false)
    }

    /**
     * Delays everything that follows by [ms] (audio goes out later, so the clock steps back by [ms]).
     * Returns the number of silence frames the caller must write now.
     */
    @Synchronized
    fun insertDelay(ms: Double): Int {
        if (segs.isEmpty()) return 0
        val exp = mediaAtLocked(written)
        val n = msToFrames(ms).toInt()
        segs.add(Seg(written, exp - ms))
        written += n
        return n
    }

    /** Media time of the sample frame with index [frame] (counted from the first frame written to the track). */
    @Synchronized
    fun mediaAt(frame: Long): Double? = if (segs.isEmpty()) null else mediaAtLocked(frame)

    @Synchronized
    fun writtenFrames() = written

    companion object {
        const val TOL_MS = 8.0
        const val MAX_GAP_MS = 250.0
    }
}

/**
 * Where the sender's clock is, judged only from packet arrivals: sender_time = now - (smallest arrival offset seen
 * in the last ~4 s). The smallest offset is the best-case network delay, so jitter does not move the clock.
 */
class ArrivalClock {
    private val ids = LongArray(4) { -1 }
    private val mins = LongArray(4) { Long.MAX_VALUE }
    private var any = false

    @Synchronized
    fun onTimestamp(senderMs: Long, nowMs: Long) {
        val off = nowMs - senderMs
        val b = nowMs / 1000
        val i = (b % 4).toInt()
        if (ids[i] != b) { ids[i] = b; mins[i] = Long.MAX_VALUE }
        if (off < mins[i]) mins[i] = off
        any = true
    }

    /** Estimated sender time right now (ms), or null before the first packet. */
    @Synchronized
    fun rawMs(nowMs: Long): Double? {
        if (!any) return null
        val b = nowMs / 1000
        var m = Long.MAX_VALUE
        for (i in 0 until 4) if (ids[i] >= b - 3 && mins[i] < m) m = mins[i]
        return if (m == Long.MAX_VALUE) null else (nowMs - m).toDouble()
    }
}

/** The common playback clock: the audio output when it is running, otherwise packet arrivals shifted to match it. */
class MediaClock(private val audioClock: () -> Double?) {
    val arrival = ArrivalClock()
    private var bias = -DEFAULT_DELAY_MS // audio clock minus arrival clock
    private var biasValid = false

    /** Sender media time that should be on screen right now. [nowMs] is a monotonic clock in ms. */
    @Synchronized
    fun nowMs(nowMs: Long): Double? {
        val raw = arrival.rawMs(nowMs) ?: return null
        val a = audioClock()
        if (a != null) {
            val b = a - raw
            bias = if (biasValid) bias + (b - bias) * 0.05 else b
            biasValid = true
            return a
        }
        return raw + bias
    }

    companion object { const val DEFAULT_DELAY_MS = 100.0 }
}

object VideoPacer {
    enum class Kind { SHOW_NOW, WAIT, DROP }
    class Decision(val kind: Kind, val waitMs: Long = 0)

    const val LATE_DROP_MS = 100.0
    const val MAX_HOLD_MS = 250.0
    const val MAX_CONSECUTIVE_DROPS = 6

    /** What to do with a decoded frame stamped [ptsMs] when the playback clock says [clockMs]. */
    fun decide(ptsMs: Double, clockMs: Double, consecutiveDrops: Int): Decision {
        val diff = ptsMs - clockMs
        if (diff < -LATE_DROP_MS && consecutiveDrops < MAX_CONSECUTIVE_DROPS) return Decision(Kind.DROP)
        if (diff > 1.0) return Decision(Kind.WAIT, Math.min(diff, MAX_HOLD_MS).toLong())
        return Decision(Kind.SHOW_NOW)
    }
}

/**
 * If the picture keeps arriving later than the sound that belongs to it (slow decoder, busy Wi-Fi), the only way to
 * keep lip-sync is to play the sound later: asks for that much extra audio delay (at most [MAX_EXTRA_MS] in total).
 */
class SyncController(private val delayAudio: (Int) -> Unit) {
    private var windowStart = -1L
    private var sum = 0.0
    private var count = 0
    private var extra = 0
    private var quietUntil = 0L

    fun onFrame(latenessMs: Double, nowMs: Long) {
        if (windowStart < 0) windowStart = nowMs
        sum += Math.max(0.0, latenessMs)
        count++
        if (nowMs - windowStart < WINDOW_MS) return
        val mean = if (count > 0) sum / count else 0.0
        val enough = count >= 5
        windowStart = nowMs; sum = 0.0; count = 0
        if (nowMs < quietUntil || !enough || mean < LATE_MEAN_MS || extra >= MAX_EXTRA_MS) return
        val d = Math.min(mean.toInt(), MAX_EXTRA_MS - extra)
        if (d <= 0) return
        extra += d
        quietUntil = nowMs + COOLDOWN_MS
        delayAudio(d)
    }

    val extraDelayMs get() = extra

    companion object {
        const val WINDOW_MS = 1000L
        const val COOLDOWN_MS = 1500L
        const val LATE_MEAN_MS = 35.0
        const val MAX_EXTRA_MS = 150
    }
}
