package ru.skreenshare.proektor

import org.junit.Assert.*
import org.junit.Test

class ProtoTest {
    private fun hex(b: ByteArray) = b.joinToString("") { "%02x".format(it) }

    // Same golden strings as the Rust tests in proto/src/lib.rs.
    @Test fun goldenHello() {
        val b = Proto.hello(Proto.Hello(Proto.PORT, 1920, 1080, 60, false, "Proj"))
        assertEquals("d7010000000001b8ba800738043c000450726f6a", hex(b))
        val h = Proto.parseHello(b, b.size)!!
        assertEquals("Proj", h.name); assertEquals(Proto.PORT, h.port); assertEquals(60, h.maxFps)
    }

    @Test fun goldenStart() {
        val b = Proto.writeStart(0x01020304, Proto.Start(1920, 1080, 30, 1, 48000, 2, 1, Proto.FEAT_NACK or Proto.FEAT_AUDIO_RED))
        assertEquals("d70304030201" + "8007" + "3804" + "1e" + "01" + "80bb0000" + "02" + "01" + "03", hex(b))
        assertEquals(0x01020304, Proto.session(b))
        val s = Proto.parseStart(b, b.size)!!
        assertEquals(1920, s.width); assertEquals(48000, s.audioRate); assertEquals(1, s.clockSync); assertEquals(3, s.features)
        // a v0.3 sender's 12-byte START: no features
        val v3 = Proto.parseStart(b, b.size - 1)!!
        assertEquals(1, v3.clockSync); assertEquals(0, v3.features)
    }

    @Test fun goldenStartAckNackAudioRed() {
        assertEquals("d704" + "01000000" + "03", hex(Proto.startAck(1, 3)))
        val items = listOf(Proto.NackItem(5, 1), Proto.NackItem(0x01020304, Proto.NACK_ALL))
        val n = Proto.nack(1, items)
        assertEquals("d725" + "01000000" + "02" + "050000000100" + "04030201ffff", hex(n))
        val back = Proto.parseNack(n, n.size)
        assertEquals(2, back.size); assertEquals(5, back[0].frameId); assertEquals(1, back[0].idx)
        assertEquals(0x01020304, back[1].frameId); assertEquals(Proto.NACK_ALL, back[1].idx)
        assertEquals(1, Proto.parseNack(n, n.size - 3).size) // truncated: only whole items
        val red = Proto.writeAudioRed(7, 9, 1234, byteArrayOf(1, 2), byteArrayOf(3, 4))
        assertEquals("d712" + "07000000" + "09000000" + "d2040000" + "0200" + "0102" + "0304", hex(red))
        val r = Proto.parseAudioRed(red, red.size)!!
        assertEquals(9, r.seq); assertEquals(1234, r.tsMs); assertEquals(2, r.prevLen); assertEquals(2, r.curLen)
        assertEquals(1, red[r.prevOffset].toInt()); assertEquals(3, red[r.curOffset].toInt())
        val first = Proto.writeAudioRed(7, 0, 0, ByteArray(0), byteArrayOf(3, 4)) // first packet: no previous chunk
        assertEquals(0, Proto.parseAudioRed(first, first.size)!!.prevLen)
        assertNull(Proto.parseAudioRed(red, red.size - 1)) // neither cur_len nor 2*cur_len bytes
    }

    @Test fun startFromV01SenderHasNoClockSync() {
        // 11-byte START as sent by v0.1.0 (same bytes as the Rust test).
        val legacy = Proto.writeStart(1, Proto.Start(1920, 1080, 30, 1, 48000, 2, 1)).copyOf(6 + 11)
        val s = Proto.parseStart(legacy, legacy.size)!!
        assertEquals(1920, s.width); assertEquals(48000, s.audioRate); assertEquals(0, s.clockSync)
    }

    @Test fun goldenVideoAudio() {
        val v = Proto.writeVideo(7, 1000, 5, 1, 3, true, byteArrayOf(0xaa.toByte(), 0xbb.toByte()))
        assertEquals("d710" + "07000000" + "e8030000" + "05000000" + "0100" + "0300" + "01" + "aabb", hex(v))
        val f = Proto.parseVideo(v, v.size)!!
        assertEquals(5, f.frameId); assertEquals(1, f.idx); assertEquals(3, f.count); assertTrue(f.key)
        val a = Proto.writeAudio(7, 9, 1234, byteArrayOf(1, 2))
        assertEquals("d711" + "07000000" + "09000000" + "d2040000" + "0102", hex(a))
        val p = Proto.parseAudio(a, a.size)!!
        assertEquals(9, p.seq); assertEquals(1234, p.tsMs)
    }

    @Test fun goldenReportPing() {
        assertEquals("d724" + "01000000" + "1e00" + "0200" + "0100" + "0500" + "0400", hex(Proto.report(1, Proto.Report(30, 2, 1, 5, 4))))
        assertEquals("d721" + "01000000" + "0d0c0b0a", hex(Proto.ping(Proto.T_PING, 1, 0x0a0b0c0d)))
        assertEquals("d72001000000", hex(Proto.simple(Proto.T_KEYREQ, 1)))
    }

    private class Rec : Reassembler.Sink {
        val frames = ArrayList<Pair<Int, String>>()
        var lost = 0
        override fun onFrame(frameId: Int, tsMs: Int, key: Boolean, data: ByteArray) { frames.add(frameId to String(data)) }
        override fun onLost() { lost++ }
    }

    private fun push(r: Reassembler, now: Long, id: Int, idx: Int, count: Int, key: Boolean, data: String) {
        val pkt = Proto.writeVideo(1, id * 33, id, idx, count, key, data.toByteArray())
        r.push(now, Proto.parseVideo(pkt, pkt.size)!!, pkt, pkt.size)
    }

    private fun poll(r: Reassembler, now: Long): List<Pair<Int, Int>> {
        val l = ArrayList<Proto.NackItem>(); r.poll(now, l); return l.map { it.frameId to it.idx }
    }

    @Test fun reassembleReordered() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 1, 2, true, "BB"); push(r, 0, 0, 0, 2, true, "AA"); push(r, 0, 1, 0, 1, false, "C")
        assertEquals(listOf(0 to "AABB", 1 to "C"), rec.frames); assertEquals(0, rec.lost)
        assertTrue(poll(r, 5).isEmpty()) // reordering alone is never reported
    }

    @Test fun missingPacketIsNackedAfterReorderDelayAndRecovered() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 0, 1, true, "K")
        push(r, 10, 1, 0, 3, false, "a"); push(r, 10, 1, 2, 3, false, "c")
        assertTrue(poll(r, 12).isEmpty())            // 2 ms: maybe just reordered
        assertEquals(listOf(1 to 1), poll(r, 19))    // only the hole, not the frame
        assertTrue(poll(r, 25).isEmpty())            // not asked again right away
        push(r, 30, 1, 1, 3, false, "b")             // the retransmission
        assertEquals(listOf(0 to "K", 1 to "abc"), rec.frames)
        assertEquals(0, rec.lost); assertEquals(1, r.nackedPkts); assertEquals(1, r.recoveredPkts)
    }

    @Test fun wholeMissingFrameIsNackedAsAll() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 0, 1, true, "K")
        push(r, 10, 2, 0, 1, false, "P")
        assertEquals(listOf(1 to Proto.NACK_ALL), poll(r, 20))
        push(r, 22, 1, 0, 1, false, "x")
        assertEquals(listOf(0 to "K", 1 to "x", 2 to "P"), rec.frames); assertEquals(0, rec.lost)
    }

    @Test fun unrecoverableFrameIsGivenUpAfterTheBudget() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 0, 1, true, "K")
        push(r, 10, 1, 0, 2, false, "x"); push(r, 12, 2, 0, 1, false, "P")
        poll(r, 30)
        assertEquals(0, rec.lost)
        poll(r, 215) // 200 ms budget passed
        assertEquals(1, rec.lost); assertEquals(1, r.lost)
        assertEquals(listOf(0 to "K", 2 to "P"), rec.frames)
    }

    @Test fun withoutNackItGivesUpQuickly() {
        val rec = Rec(); val r = Reassembler(rec, Reassembler.Config.withoutNack())
        push(r, 0, 0, 0, 1, true, "K")
        push(r, 10, 1, 0, 2, false, "x"); push(r, 12, 2, 0, 1, false, "P")
        assertTrue(poll(r, 30).isEmpty())
        poll(r, 55)
        assertEquals(1, rec.lost); assertEquals(listOf(0 to "K", 2 to "P"), rec.frames)
    }

    @Test fun duplicatesAndOldIgnored() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 5, 0, 1, true, "K"); push(r, 0, 5, 0, 1, true, "K"); push(r, 0, 3, 0, 1, false, "old")
        assertEquals(1, rec.frames.size)
    }

    @Test fun keyPendingWhileKeyframeIsIncomplete() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 0, 2, true, "a")
        assertTrue(r.keyPending())
        push(r, 1, 0, 1, 2, true, "b")
        assertFalse(r.keyPending())
    }

    @Test fun wraparound() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, -1, 0, 1, true, "a"); push(r, 0, 0, 0, 1, false, "b")
        assertEquals(2, rec.frames.size); assertEquals(0, rec.lost)
    }

    @Test fun keyPolicyHasCooldownBackoffAndNoRequestWhileKeyArrives() {
        val k = KeyPolicy()
        assertFalse(k.poll(0, false))                 // nothing needed
        k.need()
        assertTrue(k.poll(100, false))
        assertFalse(k.poll(300, false))               // cooldown 500
        assertFalse(k.poll(700, true))                // a keyframe is arriving
        assertTrue(k.poll(700, false))                // unanswered: the next wait doubles to 1000
        assertFalse(k.poll(1500, false)); assertTrue(k.poll(1700, false))
        k.onKey(); k.need()
        assertFalse(k.poll(1800, false))              // a delivered key resets the backoff to 500
        assertTrue(k.poll(2300, false))
    }

    @Test fun keyPolicyBackoffIsCapped() {
        val k = KeyPolicy(500, 4000); k.need()
        var t = 0L; var sent = 0
        while (t < 60_000) { if (k.poll(t, false)) sent++; t += 10 }
        assertTrue("sent $sent", sent in 10..20) // never a loop: far below one per 150 ms (400)
    }

    @Test fun audioRxRebuildsASingleLostPacketFromTheNextOne() {
        val rx = AudioRx(48000, 2); val out = ArrayList<AudioRx.Chunk>()
        val a = ByteArray(576); val b = ByteArray(576); val c = ByteArray(576) // 3 ms each
        rx.onRed(1, 1000, null, a, out)
        rx.onRed(3, 1006, b, c, out) // seq 2 was lost: its copy is in seq 3
        assertEquals(3, out.size)
        assertEquals(1003.0, out[1].tsMs, 0.001); assertEquals(1006.0, out[2].tsMs, 0.001)
        assertEquals(1, rx.recovered); assertEquals(0, rx.lost)
        out.clear(); rx.onRed(2, 1003, a, b, out) // late duplicate
        assertTrue(out.isEmpty())
        rx.onRed(6, 1015, b, c, out) // 4 and 5 lost, only 5 can be rebuilt
        assertEquals(1, rx.lost); assertEquals(2, rx.recovered)
    }
}
