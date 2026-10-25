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
    /** v0.4: audio packet that also carries a copy of the previous chunk. */
    const val T_AUDIO_RED = 0x12
    /** v0.5: Opus audio, 10 ms frames, each datagram carries the newest frame and a few before it. */
    const val T_AUDIO_OPUS = 0x13
    const val T_KEYREQ = 0x20
    const val T_PING = 0x21
    const val T_PONG = 0x22
    const val T_BYE = 0x23
    const val T_REPORT = 0x24
    /** v0.4: receiver -> sender, list of missing video packets. */
    const val T_NACK = 0x25

    const val FLAG_KEY = 1
    /** Set by the sender on a re-sent video packet (ignored here). */
    const val FLAG_RETX = 2
    const val FEAT_NACK = 1
    const val FEAT_AUDIO_RED = 2
    /** v0.5: this receiver decodes Opus (it passed a decode self-test on this device). */
    const val FEAT_OPUS = 4
    const val OPUS_FRAME_MS = 10
    const val NACK_ALL = 0xFFFF
    const val MAX_NACK_ITEMS = 150
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

    class Start(val width: Int, val height: Int, val fps: Int, val vcodec: Int, val audioRate: Int, val audioChannels: Int,
                /** 1: audio timestamps share the video clock (lip-sync possible). */
                val clockSync: Int = 0,
                /** v0.4 capability bits the sender offers (FEAT_*); 0 from older senders. */
                val features: Int = 0)

    fun writeStart(session: Int, s: Start): ByteArray =
        header(T_START, session).u16(s.width).u16(s.height).u8(s.fps).u8(s.vcodec).i32(s.audioRate)
            .u8(s.audioChannels).u8(s.clockSync).u8(s.features).toByteArray()

    fun parseStart(b: ByteArray, len: Int): Start? {
        val o = HEADER_LEN
        if (len < o + 11) return null
        return Start(u16(b, o), u16(b, o + 2), u8(b, o + 4), u8(b, o + 5), i32(b, o + 6), u8(b, o + 10),
            if (len >= o + 12) u8(b, o + 11) else 0,
            if (len >= o + 13) u8(b, o + 12) else 0)
    }

    /** START_ACK: the features we accepted (what the sender offered AND we can do). A v0.3 ACK has no body. */
    fun startAck(session: Int, features: Int): ByteArray = header(T_START_ACK, session).u8(features).toByteArray()

    fun simple(type: Int, session: Int): ByteArray = header(type, session).toByteArray()
    fun ping(type: Int, session: Int, t: Int): ByteArray = header(type, session).i32(t).toByteArray()

    /** Per-second figures. [nackPkts]: video packets that had to be asked for again; [recoveredPkts]: of those, how many arrived. */
    class Report(val decodedFps: Int, val lostFrames: Int, val droppedFrames: Int, val nackPkts: Int = 0, val recoveredPkts: Int = 0,
                 /** v0.5: frames received whole (before decoding) and frames put on the screen, per second. */
                 val receivedFps: Int = 0, val shownFps: Int = 0)

    fun report(session: Int, r: Report): ByteArray =
        header(T_REPORT, session).u16(r.decodedFps).u16(r.lostFrames).u16(r.droppedFrames)
            .u16(r.nackPkts).u16(r.recoveredPkts).u16(r.receivedFps).u16(r.shownFps).toByteArray()

    class NackItem(val frameId: Int, val idx: Int)

    /** NACK: count(1) + count x (frame_id u32, idx u16). Items beyond [MAX_NACK_ITEMS] are not written. */
    fun nack(session: Int, items: List<NackItem>): ByteArray {
        val n = minOf(items.size, MAX_NACK_ITEMS)
        val w = header(T_NACK, session).u8(n)
        for (i in 0 until n) w.i32(items[i].frameId).u16(items[i].idx)
        return w.toByteArray()
    }

    fun parseNack(b: ByteArray, len: Int): List<NackItem> {
        if (len <= HEADER_LEN) return emptyList()
        val n = u8(b, HEADER_LEN)
        val out = ArrayList<NackItem>()
        var o = HEADER_LEN + 1
        while (out.size < n && o + 6 <= len) { out.add(NackItem(i32(b, o), u16(b, o + 4))); o += 6 }
        return out
    }

    /** Header of a VIDEO datagram; fields start right after the 6-byte header. */
    class VideoFrag(val tsMs: Int, val frameId: Int, val idx: Int, val count: Int, val key: Boolean,
                    val payloadOffset: Int)

    const val VIDEO_HEADER_LEN = HEADER_LEN + 13 // = 19 (ts 4, frame 4, idx 2, count 2, flags 1)

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

    /** AUDIO_OPUS: seq(4) ts_ms(4) n(1) + n x (len u16, Opus packet); frame k is sequence seq-k and starts k*10 ms earlier. */
    class AudioOpus(val seq: Int, val tsMs: Int, val frames: List<ByteArray>)

    fun writeAudioOpus(session: Int, seq: Int, tsMs: Int, frames: List<ByteArray>): ByteArray {
        val w = header(T_AUDIO_OPUS, session).i32(seq).i32(tsMs).u8(frames.size)
        for (f in frames) w.u16(f.size).bytes(f)
        return w.toByteArray()
    }

    /** Null for anything malformed (truncated, no frames, more than 8 frames) - never throws. */
    fun parseAudioOpus(b: ByteArray, len: Int): AudioOpus? {
        val o = HEADER_LEN
        if (len < o + 9) return null
        val n = u8(b, o + 8)
        if (n == 0 || n > 8) return null
        var at = o + 9
        val frames = ArrayList<ByteArray>(n)
        for (i in 0 until n) {
            if (at + 2 > len) return null
            val l = u16(b, at)
            at += 2
            if (at + l > len) return null
            frames.add(b.copyOfRange(at, at + l))
            at += l
        }
        return AudioOpus(i32(b, o), i32(b, o + 4), frames)
    }

    const val AUDIO_RED_HEADER_LEN = HEADER_LEN + 10

    /** AUDIO_RED: seq(4) ts_ms(4) cur_len(2), then [previous chunk (0 or cur_len bytes)][current chunk]; ts is of the CURRENT chunk. */
    class AudioRed(val seq: Int, val tsMs: Int, val prevOffset: Int, val prevLen: Int, val curOffset: Int, val curLen: Int)

    fun writeAudioRed(session: Int, seq: Int, tsMs: Int, prev: ByteArray, cur: ByteArray): ByteArray =
        header(T_AUDIO_RED, session).i32(seq).i32(tsMs).u16(cur.size).bytes(prev).bytes(cur).toByteArray()

    fun parseAudioRed(b: ByteArray, len: Int): AudioRed? {
        if (len < AUDIO_RED_HEADER_LEN) return null
        val o = HEADER_LEN
        val curLen = u16(b, o + 8)
        val data = len - AUDIO_RED_HEADER_LEN
        if (curLen == 0 || data < curLen || (data != curLen && data != 2 * curLen)) return null
        val prevLen = data - curLen
        return AudioRed(i32(b, o), i32(b, o + 4), AUDIO_RED_HEADER_LEN, prevLen, AUDIO_RED_HEADER_LEN + prevLen, curLen)
    }
}

/**
 * Puts VIDEO fragments back together (mirror of proto/src/reassembly.rs; the Rust tests and the loopback simulation
 * there are the reference). Fragments are identified by (frame_id, idx). Frames come out strictly in order.
 *
 * A missing fragment is not a reason to ask for a keyframe: after a short reorder delay [poll] lists exactly the
 * missing fragments (a NACK) and the sender re-sends only those. Only a frame still incomplete after the latency
 * budget is given up ([Sink.onLost], the decoder then needs a keyframe, see [KeyPolicy]).
 * Time is passed in (any monotonic ms clock) so this is plain-JVM testable.
 */
class Reassembler(private val sink: Sink, private val cfg: Config = Config()) {
    interface Sink {
        fun onFrame(frameId: Int, tsMs: Int, key: Boolean, data: ByteArray)
        fun onLost()
    }

    class Config(
        /** A hole is only reported after this long (packets may just be reordered). */
        val reorderMs: Long = 8,
        /** Ask again for a still-missing fragment after this long. */
        val retryMs: Long = 40,
        val maxTries: Int = 6,
        /** A frame that blocks the stream for this long is given up: the latency we are willing to add. */
        val budgetMs: Long = 200,
        val maxPending: Int = 64,
        /** False for a sender that cannot re-send (v0.3): never NACK, give up quickly. */
        val nack: Boolean = true,
    ) {
        companion object { fun withoutNack() = Config(nack = false, budgetMs = 40) }
    }

    private class Partial(val tsMs: Int, val key: Boolean, val count: Int, now: Long) {
        val frags = arrayOfNulls<ByteArray>(count)
        var got = 0
        val firstMs = now
        var lastMs = now
        var high = -1
        val evidentAt = LongArray(count) { Long.MAX_VALUE }
        val nackAt = LongArray(count) { Long.MAX_VALUE }
        val tries = IntArray(count)
    }

    private var hasNext = false
    private var nextId = 0
    private val pending = HashMap<Int, Partial>()
    /** Whole frames of which nothing arrived: frame -> (last NACK time, tries). */
    private val gapNack = HashMap<Int, LongArray>()

    var lost = 0
        private set
    /** Fragments reported missing (each counted once) and of those how many showed up afterwards. */
    var nackedPkts = 0
        private set
    var recoveredPkts = 0
        private set

    fun reset() { hasNext = false; pending.clear(); gapNack.clear() }

    fun hasPending() = pending.isNotEmpty()

    /** A keyframe is being received right now (asking for another one would only add load). */
    fun keyPending() = pending.values.any { it.key }

    fun push(nowMs: Long, f: Proto.VideoFrag, buf: ByteArray, len: Int) {
        if (f.count == 0 || f.idx >= f.count) return
        if (!hasNext) { hasNext = true; nextId = f.frameId }
        if (f.frameId - nextId < 0) return // already delivered or skipped
        if (gapNack.remove(f.frameId) != null) recoveredPkts++
        val p = pending.getOrPut(f.frameId) { Partial(f.tsMs, f.key, f.count, nowMs) }
        if (p.count != f.count) return
        val i = f.idx
        if (p.frags[i] != null) return // duplicate
        p.frags[i] = buf.copyOfRange(f.payloadOffset, len)
        p.got++
        p.lastMs = nowMs
        if (p.nackAt[i] != Long.MAX_VALUE) recoveredPkts++
        if (i > p.high) {
            for (j in p.high + 1 until i) if (p.frags[j] == null && p.evidentAt[j] == Long.MAX_VALUE) p.evidentAt[j] = nowMs
            p.high = i
        }
        drain()
        while (pending.size > cfg.maxPending) giveUpHead()
    }

    /**
     * Call every few ms. Appends the fragments to ask the sender for ([Proto.NACK_ALL] as idx = a whole frame)
     * and gives up on frames that exceeded the latency budget.
     */
    fun poll(nowMs: Long, nacks: MutableList<Proto.NackItem>) {
        if (!hasNext || pending.isEmpty()) return
        while (true) {
            val head = firstPending() ?: break
            if (nowMs - pending[head]!!.firstMs >= cfg.budgetMs) giveUpHead() else break
        }
        if (!cfg.nack) return
        val head = firstPending() ?: return
        // frames of which nothing arrived
        if (head != nextId && nowMs - pending[head]!!.firstMs >= cfg.reorderMs) {
            val n = minOf(head - nextId, 32)
            for (k in 0 until n) {
                val id = nextId + k
                val e = gapNack.getOrPut(id) { longArrayOf(0, 0) }
                if (e[1] < cfg.maxTries && (e[1] == 0L || nowMs - e[0] >= cfg.retryMs)) {
                    if (e[1] == 0L) nackedPkts++
                    e[0] = nowMs; e[1]++
                    nacks.add(Proto.NackItem(id, Proto.NACK_ALL))
                }
            }
        }
        // holes inside frames we have started to receive (in stream order)
        for (id in pending.keys.sortedBy { it - nextId }) {
            val p = pending[id]!!
            if (p.got == p.count) continue
            val tailQuiet = nowMs - p.lastMs >= cfg.reorderMs
            for (i in 0 until p.count) {
                if (p.frags[i] != null || p.tries[i] >= cfg.maxTries) continue
                val evident = if (i < p.high) p.evidentAt[i] != Long.MAX_VALUE && nowMs - p.evidentAt[i] >= cfg.reorderMs else tailQuiet
                if (!evident) continue
                if (p.tries[i] == 0 || nowMs - p.nackAt[i] >= cfg.retryMs) {
                    if (p.tries[i] == 0) nackedPkts++
                    p.tries[i]++
                    p.nackAt[i] = nowMs
                    nacks.add(Proto.NackItem(id, i))
                }
            }
        }
    }

    private fun firstPending(): Int? {
        var best: Int? = null
        for (id in pending.keys) if (best == null || id - best < 0) best = id
        return best
    }

    /** The stream is blocked on a hole: abandon what blocks it (missing frames, or the first incomplete frame). */
    private fun giveUpHead() {
        val head = firstPending() ?: return
        if (head != nextId) {
            lost += head - nextId // frames that never showed up at all
            nextId = head
        } else {
            pending.remove(head)
            lost++
            nextId = head + 1
        }
        sink.onLost()
        drain()
    }

    private fun drain() {
        while (true) {
            val p = pending[nextId]
            if (p == null || p.got != p.count) break
            pending.remove(nextId)
            var total = 0
            for (fr in p.frags) total += fr!!.size
            val data = ByteArray(total)
            var o = 0
            for (fr in p.frags) { System.arraycopy(fr!!, 0, data, o, fr.size); o += fr.size }
            val id = nextId
            nextId = id + 1
            sink.onFrame(id, p.tsMs, p.key, data)
        }
        val it = gapNack.keys.iterator()
        while (it.hasNext()) if (it.next() - nextId < 0) it.remove()
    }
}
