package ru.skreenshare.proektor

import org.junit.Assert.*
import org.junit.Test

class SyncTest {
    private val rate = 48000
    private fun frames(ms: Int) = rate * ms / 1000
    private fun near(a: Int, b: Int) = assertEquals(a.toDouble(), b.toDouble(), 2.0)

    @Test fun timelineIsLinearWithinASegment() {
        val t = AudioTimeline(rate)
        for (i in 0 until 10) {
            val p = t.plan(1000.0 + i * 5, frames(5), false)
            assertEquals(0, p.silenceFrames); assertFalse(p.drop)
        }
        assertEquals(1000.0, t.mediaAt(0)!!, 0.01)
        assertEquals(1025.0, t.mediaAt(frames(25).toLong())!!, 0.01)
    }

    @Test fun smallJitterIsIgnored() {
        val t = AudioTimeline(rate)
        t.plan(1000.0, frames(5), true)
        val p = t.plan(1000.0 + 5 + 6, frames(5), false) // 6 ms late: inside the tolerance
        assertEquals(0, p.silenceFrames); assertEquals(0, p.skipFrames); assertFalse(p.drop)
    }

    @Test fun lostPacketsBecomeSilenceSoTheClockStaysTrue() {
        val t = AudioTimeline(rate)
        t.plan(1000.0, frames(5), true)
        val p = t.plan(1025.0, frames(5), false) // 20 ms missing
        near(frames(20), p.silenceFrames)
        // the chunk now sits at the frame that corresponds to its stamp
        val startOfChunk = (frames(5) + p.silenceFrames).toLong()
        assertEquals(1025.0, t.mediaAt(startOfChunk)!!, 0.1)
    }

    @Test fun overlapIsTrimmedOrDropped() {
        val t = AudioTimeline(rate)
        t.plan(1000.0, frames(10), true)           // covers 1000..1010
        val trim = t.plan(1004.0, frames(10), false) // starts 6 ms early -> inside tolerance, kept
        assertEquals(0, trim.skipFrames)
        val t2 = AudioTimeline(rate)
        t2.plan(1000.0, frames(30), true)           // covers 1000..1030
        val p = t2.plan(1010.0, frames(10), false)   // fully inside what is written
        assertTrue(p.drop)
        val p2 = t2.plan(1020.0, frames(20), false)  // 10 ms overlap
        near(frames(10), p2.skipFrames)
    }

    @Test fun bigJumpStartsANewSegment() {
        val t = AudioTimeline(rate)
        t.plan(1000.0, frames(5), true)
        val p = t.plan(5000.0, frames(5), false)
        assertEquals(0, p.silenceFrames)
        assertEquals(5000.0, t.mediaAt(frames(5).toLong())!!, 0.01)
    }

    @Test fun insertedDelayStepsTheClockBack() {
        val t = AudioTimeline(rate)
        t.plan(1000.0, frames(100), true) // media 1000..1100 written
        val n = t.insertDelay(60.0)
        assertEquals(frames(60), n)
        val w = t.writtenFrames()
        assertEquals(1100.0 - 60.0, t.mediaAt(frames(100).toLong())!!, 0.01)
        // after the silence the next chunk (stamp 1100) lines up without any extra filling
        val p = t.plan(1100.0, frames(5), false)
        assertEquals(0, p.silenceFrames); assertEquals(0, p.skipFrames)
        assertEquals(1100.0, t.mediaAt(w)!!, 0.5)
    }

    @Test fun arrivalClockUsesBestCaseDelay() {
        val c = ArrivalClock()
        assertNull(c.rawMs(1000))
        // sender time 0..: packets arrive 30 ms after stamp, one of them 80 ms late
        c.onTimestamp(0, 30); c.onTimestamp(10, 40); c.onTimestamp(20, 100); c.onTimestamp(30, 60)
        assertEquals(100.0 - 30.0, c.rawMs(100)!!, 0.001)
    }

    @Test fun arrivalClockForgetsOldOffsets() {
        val c = ArrivalClock()
        c.onTimestamp(0, 10)          // offset 10
        c.onTimestamp(10_000, 10_050) // much later: offset 50
        assertEquals(10_100.0 - 50.0, c.rawMs(10_100)!!, 0.001)
    }

    @Test fun mediaClockPrefersAudioAndKeepsContinuityWhenItStops() {
        var audio: Double? = null
        val clock = MediaClock { audio }
        // no packets yet
        assertNull(clock.nowMs(0))
        clock.arrival.onTimestamp(0, 20)
        // no audio: arrival clock with the default 100 ms of latency
        assertEquals(1000.0 - 20 - MediaClock.DEFAULT_DELAY_MS, clock.nowMs(1000)!!, 0.01)
        // audio runs 150 ms behind the arrival clock
        audio = 1000.0 - 20 - 150
        assertEquals(audio, clock.nowMs(1000)!!, 0.01)
        // audio goes away: the arrival clock keeps the same 150 ms offset, so the video does not jump
        audio = null
        assertEquals(2000.0 - 20 - 150, clock.nowMs(2000)!!, 0.01)
    }

    @Test fun pacerWaitsShowsAndDrops() {
        assertEquals(VideoPacer.Kind.WAIT, VideoPacer.decide(1050.0, 1000.0, 0).kind)
        assertEquals(50L, VideoPacer.decide(1050.0, 1000.0, 0).waitMs)
        assertEquals(VideoPacer.Kind.SHOW_NOW, VideoPacer.decide(990.0, 1000.0, 0).kind)
        assertEquals(VideoPacer.Kind.DROP, VideoPacer.decide(800.0, 1000.0, 0).kind)
        // never freeze: after several drops in a row the next frame is shown anyway
        assertEquals(VideoPacer.Kind.SHOW_NOW, VideoPacer.decide(800.0, 1000.0, VideoPacer.MAX_CONSECUTIVE_DROPS).kind)
        assertEquals(250L, VideoPacer.decide(9000.0, 1000.0, 0).waitMs)
    }

    @Test fun controllerDelaysAudioWhenVideoIsLateAndBacksOff() {
        var asked = 0
        val c = SyncController { asked += it }
        var t = 0L
        repeat(40) { c.onFrame(70.0, t); t += 25 } // 1 s of frames, each 70 ms late
        c.onFrame(70.0, t)
        assertTrue("asked=$asked", asked in 60..80)
        val first = asked
        // cooldown: more late frames right away do not stack another request
        repeat(20) { c.onFrame(70.0, t); t += 25 }
        assertEquals(first, asked)
        // on-time video never asks
        val quiet = SyncController { fail() }
        t = 0
        repeat(200) { quiet.onFrame(5.0, t); t += 25 }
    }

    @Test fun controllerNeverExceedsTheCap() {
        var asked = 0
        val c = SyncController { asked += it }
        var t = 0L
        repeat(2000) { c.onFrame(120.0, t); t += 25 }
        assertTrue(asked <= SyncController.MAX_EXTRA_MS)
    }
}
