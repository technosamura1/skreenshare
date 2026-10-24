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
        val b = Proto.writeStart(0x01020304, Proto.Start(1920, 1080, 30, 1, 48000, 2))
        assertEquals("d70304030201" + "8007" + "3804" + "1e" + "01" + "80bb0000" + "02", hex(b))
        assertEquals(0x01020304, Proto.session(b))
        val s = Proto.parseStart(b, b.size)!!
        assertEquals(1920, s.width); assertEquals(48000, s.audioRate)
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
        assertEquals("d724" + "01000000" + "1e00" + "0200" + "0100", hex(Proto.report(1, Proto.Report(30, 2, 1))))
        assertEquals("d721" + "01000000" + "0d0c0b0a", hex(Proto.ping(Proto.T_PING, 1, 0x0a0b0c0d)))
        assertEquals("d72001000000", hex(Proto.simple(Proto.T_KEYREQ, 1)))
    }

    private class Rec : Reassembler.Sink {
        val frames = ArrayList<Pair<Int, String>>()
        var lost = 0
        override fun onFrame(frameId: Int, tsMs: Int, key: Boolean, data: ByteArray) { frames.add(frameId to String(data)) }
        override fun onLost() { lost++ }
    }

    private fun push(r: Reassembler, id: Int, idx: Int, count: Int, key: Boolean, data: String) {
        val pkt = Proto.writeVideo(1, id * 33, id, idx, count, key, data.toByteArray())
        r.push(Proto.parseVideo(pkt, pkt.size)!!, pkt, pkt.size)
    }

    @Test fun reassembleReordered() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 1, 2, true, "BB"); push(r, 0, 0, 2, true, "AA"); push(r, 1, 0, 1, false, "C")
        assertEquals(listOf(0 to "AABB", 1 to "C"), rec.frames); assertEquals(0, rec.lost)
    }

    @Test fun lostFragmentSkipped() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 1, true, "K"); push(r, 1, 0, 2, false, "x"); push(r, 2, 0, 1, false, "P")
        assertEquals(listOf(0 to "K", 2 to "P"), rec.frames); assertEquals(1, rec.lost); assertEquals(1, r.lost)
    }

    @Test fun duplicatesAndOldIgnored() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 5, 0, 1, true, "K"); push(r, 5, 0, 1, true, "K"); push(r, 3, 0, 1, false, "old")
        assertEquals(1, rec.frames.size)
    }

    @Test fun giveUpOldest() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, 0, 0, 2, true, "a")
        assertTrue(r.hasPending()); r.giveUpOldest()
        assertEquals(1, rec.lost); assertFalse(r.hasPending())
        push(r, 1, 0, 1, true, "k")
        assertEquals(listOf(1 to "k"), rec.frames)
    }

    @Test fun wraparound() {
        val rec = Rec(); val r = Reassembler(rec)
        push(r, -1, 0, 1, true, "a"); push(r, 0, 0, 1, false, "b")
        assertEquals(2, rec.frames.size); assertEquals(0, rec.lost)
    }
}
