package ru.skreenshare.proektor

import java.nio.charset.Charset

/**
 * Wire protocol (mirror of proto/src/lib.rs in the Rust sender). Pure Kotlin, no Android classes, so it is
 * covered by plain JVM unit tests. All integers are little-endian; u32 values are kept in Int (wrapping).
 */
object Proto {
    const val PORT = 47800
    const val DISCOVERY_PORT = 47801
    const val MDNS_TYPE = "_skreenshare._udp."
    const val VERSION = 1
    const val MAGIC = 0xD7
    const val HEADER_LEN = 6
    const val MAX_PAYLOAD = 1200

    const val T_HELLO = 0x01
    const val T_PROBE = 0x02
    const val T_START = 0x03
    const val T_START_ACK = 0x04
    const val T_VIDEO = 0x10
    const val T_AUDIO = 0x11
    const val T_KEYREQ = 0x20
    const val T_PING = 0x21
    const val T_PONG = 0x22
    const val T_BYE = 0x23
    const val T_REPORT = 0x24

    const val FLAG_KEY = 1
    const val VCODEC_H264 = 1

    fun u8(b: ByteArray, o: Int) = b[o].toInt() and 0xFF
    fun u16(b: ByteArray, o: Int) = u8(b, o) or (u8(b, o + 1) shl 8)
    fun i32(b: ByteArray, o: Int) = u8(b, o) or (u8(b, o + 1) shl 8) or (u8(b, o + 2) shl 16) or (u8(b, o + 3) shl 24)

    class Writer(cap: Int = 64) {
        private var buf = ByteArray(cap)
        var len = 0
            private set
        fun u8(v: Int): Writer { ensure(1); buf[len++] = v.toByte(); return this }
        fun u16(v: Int): Writer = u8(v).u8(v shr 8)
        fun i32(v: Int): Writer = u8(v).u8(v shr 8).u8(v shr 16).u8(v shr 24)
        fun bytes(b: ByteArray): Writer { ensure(b.size); System.arraycopy(b, 0, buf, len, b.size); len += b.size; return this }
        private fun ensure(n: Int) { if (len + n > buf.size) buf = buf.copyOf(maxOf(buf.size * 2, len + n)) }
        fun toByteArray(): ByteArray = buf.copyOf(len)
    }

    private fun header(type: Int, session: Int) = Writer().u8(MAGIC).u8(type).i32(session)

    /** Returns true if this datagram is ours. */
    fun isOurs(b: ByteArray, len: Int) = len >= HEADER_LEN && u8(b, 0) == MAGIC
    fun type(b: ByteArray) = u8(b, 1)
    fun session(b: ByteArray) = i32(b, 2)

    class Hello(val port: Int, val maxW: Int, val maxH: Int, val maxFps: Int, val busy: Boolean, val name: String)

    fun hello(h: Hello): ByteArray {
        val name = h.name.toByteArray(Charsets.UTF_8).let { if (it.size > 60) it.copyOf(60) else it }
        return header(T_HELLO, 0).u8(VERSION).u16(h.port).u16(h.maxW).u16(h.maxH).u8(h.maxFps)
            .u8(if (h.busy) 1 else 0).u8(name.size).bytes(name).toByteArray()
    }

    fun parseHello(b: ByteArray, len: Int): Hello? {
        val o = HEADER_LEN
        if (len < o + 10 || u8(b, o) != VERSION) return null
        val n = u8(b, o + 9)
        if (len < o + 10 + n) return null
        return Hello(u16(b, o + 1), u16(b, o + 3), u16(b, o + 5), u8(b, o + 7), u8(b, o + 8) and 1 != 0,
            String(b, o + 10, n, Charset.forName("UTF-8")))
    }

    /** Reply port of a PROBE, or -1. */
    fun parseProbe(b: ByteArray, len: Int): Int = if (len >= HEADER_LEN + 2) u16(b, HEADER_LEN) else -1

    class Start(val width: Int, val height: Int, val fps: Int, val vcodec: Int, val audioRate: Int, val audioChannels: Int)

    fun writeStart(session: Int, s: Start): ByteArray =
        header(T_START, session).u16(s.width).u16(s.height).u8(s.fps).u8(s.vcodec).i32(s.audioRate)
            .u8(s.audioChannels).toByteArray()

    fun parseStart(b: ByteArray, len: Int): Start? {
        val o = HEADER_LEN
        if (len < o + 11) return null
        return Start(u16(b, o), u16(b, o + 2), u8(b, o + 4), u8(b, o + 5), i32(b, o + 6), u8(b, o + 10))
    }

    fun simple(type: Int, session: Int): ByteArray = header(type, session).toByteArray()
    fun ping(type: Int, session: Int, t: Int): ByteArray = header(type, session).i32(t).toByteArray()

    class Report(val decodedFps: Int, val lostFrames: Int, val droppedFrames: Int)

    fun report(session: Int, r: Report): ByteArray =
        header(T_REPORT, session).u16(r.decodedFps).u16(r.lostFrames).u16(r.droppedFrames).toByteArray()

    /** Header of a VIDEO datagram; fields start right after the 6-byte header. */
    class VideoFrag(val tsMs: Int, val frameId: Int, val idx: Int, val count: Int, val key: Boolean,
                    val payloadOffset: Int)

    const val VIDEO_HEADER_LEN = HEADER_LEN + 13

    fun parseVideo(b: ByteArray, len: Int): VideoFrag? {
        if (len < VIDEO_HEADER_LEN) return null
        val o = HEADER_LEN
        return VideoFrag(i32(b, o), i32(b, o + 4), u16(b, o + 8), u16(b, o + 10), u8(b, o + 12) and FLAG_KEY != 0,
            VIDEO_HEADER_LEN)
    }

    fun writeVideo(session: Int, tsMs: Int, frameId: Int, idx: Int, count: Int, key: Boolean, payload: ByteArray): ByteArray =
        header(T_VIDEO, session).i32(tsMs).i32(frameId).u16(idx).u16(count).u8(if (key) FLAG_KEY else 0)
            .bytes(payload).toByteArray()

    const val AUDIO_HEADER_LEN = HEADER_LEN + 8

    class AudioPkt(val seq: Int, val tsMs: Int, val payloadOffset: Int)

    fun parseAudio(b: ByteArray, len: Int): AudioPkt? =
        if (len < AUDIO_HEADER_LEN) null else AudioPkt(i32(b, HEADER_LEN), i32(b, HEADER_LEN + 4), AUDIO_HEADER_LEN)

    fun writeAudio(session: Int, seq: Int, tsMs: Int, payload: ByteArray): ByteArray =
        header(T_AUDIO, session).i32(seq).i32(tsMs).bytes(payload).toByteArray()
}

/**
 * Puts VIDEO fragments back together. Frames come out strictly in order; a frame that can never complete is
 * reported through [Sink.onLost] (the decoder then needs a keyframe).
 */
class Reassembler(private val sink: Sink) {
    interface Sink {
        fun onFrame(frameId: Int, tsMs: Int, key: Boolean, data: ByteArray)
        fun onLost()
    }

    private class Partial(val tsMs: Int, val key: Boolean, val count: Int) {
        val frags = arrayOfNulls<ByteArray>(count)
        var got = 0
        var firstSeenNs = System.nanoTime()
    }

    private var started = false
    private var nextId = 0
    private val pending = HashMap<Int, Partial>()
    var lost = 0
        private set

    fun reset() { started = false; pending.clear() }

    fun push(f: Proto.VideoFrag, buf: ByteArray, len: Int) {
        if (f.count == 0 || f.idx >= f.count) return
        if (!started) { started = true; nextId = f.frameId }
        if (f.frameId - nextId < 0) return // already delivered or skipped
        val p = pending.getOrPut(f.frameId) { Partial(f.tsMs, f.key, f.count) }
        if (p.count != f.count) return
        if (p.frags[f.idx] == null) {
            p.frags[f.idx] = buf.copyOfRange(f.payloadOffset, len)
            p.got++
        }
        drain()
        while (pending.size > MAX_PENDING) skipOldest()
    }

    fun hasPending() = pending.isNotEmpty()

    /** True if the oldest incomplete frame has been waiting longer than [maxAgeMs]. */
    fun oldestOlderThan(maxAgeMs: Long): Boolean {
        val oldest = oldestId() ?: return false
        return (System.nanoTime() - pending[oldest]!!.firstSeenNs) / 1_000_000 > maxAgeMs
    }

    fun giveUpOldest() { if (pending.isNotEmpty()) skipOldest() }

    private fun oldestId(): Int? {
        var best: Int? = null
        for (id in pending.keys) if (best == null || id - best < 0) best = id
        return best
    }

    private fun skipOldest() {
        val id = oldestId() ?: return
        pending.remove(id)
        nextId = id + 1
        lost++
        sink.onLost()
        drain()
    }

    private fun drain() {
        while (true) {
            var cid: Int? = null
            for ((id, p) in pending) if (p.got == p.count && (cid == null || id - cid < 0)) cid = id
            if (cid == null) return
            if (cid != nextId) {
                val missing = cid - nextId
                val it = pending.keys.iterator()
                while (it.hasNext()) if (it.next() - nextId < missing) it.remove()
                lost += missing
                sink.onLost()
                nextId = cid
            }
            val p = pending.remove(cid)!!
            var total = 0
            for (fr in p.frags) total += fr!!.size
            val data = ByteArray(total)
            var o = 0
            for (fr in p.frags) { System.arraycopy(fr!!, 0, data, o, fr.size); o += fr.size }
            sink.onFrame(cid, p.tsMs, p.key, data)
            nextId = cid + 1
        }
    }

    companion object { const val MAX_PENDING = 4 }
}
