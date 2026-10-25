package ru.skreenshare.proektor

import android.content.Context
import android.media.MediaCodecList
import android.os.Build
import android.os.Process
import android.util.Log
import android.view.Surface
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.Inet4Address
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.NetworkInterface
import java.net.SocketTimeoutException

/**
 * The whole receiver: listens on UDP [Proto.PORT], announces itself, and when a PC sends START decodes its
 * H.264 video to the surface and plays its audio. Latest START wins, so no codes or pairing are needed.
 */
class Engine(private val ctx: Context, private val listener: Listener) : Reassembler.Sink {
    interface Listener {
        fun onStreaming(active: Boolean, info: String, width: Int = 0, height: Int = 0)
        fun onError(msg: String)
        /** Once a second while streaming: a line of figures for the hidden stats overlay. */
        fun onStats(text: String) {}
    }

    private class Session(val id: Int, val peer: InetSocketAddress, val start: Proto.Start, val features: Int)

    @Volatile private var running = false
    @Volatile private var surface: Surface? = null
    private var socket: DatagramSocket? = null
    private var thread: Thread? = null

    // Only touched from the network thread (except where marked).
    private var session: Session? = null
    private var decoder: VideoDecoder? = null
    @Volatile private var audio: AudioPlayer? = null
    private var clock: MediaClock? = null
    private var reasm = Reassembler(this)
    private val keyPolicy = KeyPolicy()
    private var arx: AudioRx? = null
    private var opusRx: OpusRx? = null
    private var opusDec: OpusDecoder? = null
    private val opusOut = ArrayList<OpusRx.Frame>(4)
    private var recvFrames = 0
    private var lastRecvFrames = 0
    private var lastShown = 0
    private val audioOut = ArrayList<AudioRx.Chunk>(4)
    private val nackOut = ArrayList<Proto.NackItem>()
    private var awaitingKey = true
    private var lastNacked = 0
    private var lastRecovered = 0
    private var lastPacketAt = 0L
    private var lastHello = 0L
    private var lastReport = 0L
    private var lastDecoded = 0
    private var lastDropped = 0
    private var lastLost = 0
    @Volatile private var needKey = false

    val deviceName: String = Build.MODEL?.takeIf { it.isNotBlank() } ?: "Android"
    private val caps: Caps by lazy { detectCaps() }

    class Caps(val w: Int, val h: Int, val fps: Int)

    fun setSurface(s: Surface?) {
        surface = s
        if (s == null) post { endSession(sendBye = true) }
    }

    // Tiny executor-ish: run a lambda on the network thread at its next wakeup.
    private val tasks = java.util.concurrent.ConcurrentLinkedQueue<() -> Unit>()
    private fun post(f: () -> Unit) { tasks.add(f) }

    fun start() {
        if (running) return
        running = true
        // Find out in the background whether this device decodes Opus (cached; beginSession needs the answer).
        Thread({ OpusDecoder.selfTest() }, "opus-test").also { it.isDaemon = true; it.start() }
        thread = Thread({ run() }, "net").also { it.start() }
    }

    fun stop() {
        running = false
        try { socket?.close() } catch (_: Throwable) {}
        try { thread?.join(1000) } catch (_: Throwable) {}
        thread = null
    }

    private fun run() {
        // A busy projector must not let the Wi-Fi packets wait behind other work.
        try { Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY) } catch (_: Throwable) {}
        val sock = try {
            DatagramSocket(null).apply {
                reuseAddress = true
                bind(InetSocketAddress(Proto.PORT))
                broadcast = true
                // 8 MB: a keyframe burst (hundreds of packets) must fit while this thread is busy. The system may give less.
                try { receiveBufferSize = 8 shl 20 } catch (_: Throwable) {}
                Log.i(TAG, "udp receive buffer: ${try { receiveBufferSize } catch (_: Throwable) { -1 }}")
                soTimeout = 3 // short: NACKs and the loss timers are driven from this loop
            }
        } catch (e: Throwable) {
            listener.onError("Не удалось открыть порт ${Proto.PORT}: $e")
            running = false
            return
        }
        socket = sock
        val buf = ByteArray(2048)
        val pkt = DatagramPacket(buf, buf.size)
        while (running) {
            try {
                pkt.length = buf.size
                sock.receive(pkt)
                handle(sock, buf, pkt.length, pkt.address, pkt.port)
            } catch (_: SocketTimeoutException) {
            } catch (e: Throwable) {
                if (!running) break
                Log.w(TAG, "net: $e")
            }
            try { housekeeping(sock) } catch (e: Throwable) { Log.w(TAG, "tick: $e") }
        }
        endSession(sendBye = true)
        try { sock.close() } catch (_: Throwable) {}
    }

    private fun send(sock: DatagramSocket, data: ByteArray, to: InetSocketAddress) {
        try { sock.send(DatagramPacket(data, data.size, to)) } catch (_: Throwable) {}
    }

    private fun handle(sock: DatagramSocket, b: ByteArray, len: Int, from: InetAddress, port: Int) {
        if (!Proto.isOurs(b, len)) return
        val sid = Proto.session(b)
        val now = System.nanoTime()
        val nowMs = now / 1_000_000
        when (Proto.type(b)) {
            Proto.T_PROBE -> {
                val rp = Proto.parseProbe(b, len)
                if (rp > 0) send(sock, helloBytes(), InetSocketAddress(from, rp))
            }
            Proto.T_START -> {
                val s = Proto.parseStart(b, len) ?: return
                val cur = session
                val peer = InetSocketAddress(from, port)
                if (cur != null && cur.id == sid && cur.peer == peer) {
                    send(sock, Proto.startAck(sid, cur.features), peer) // sender retries until it hears this
                    lastPacketAt = now
                    return
                }
                beginSession(sock, sid, peer, s)
            }
            else -> {
                val cur = session ?: return
                if (cur.id != sid || cur.peer.address != from) return
                lastPacketAt = now
                when (Proto.type(b)) {
                    Proto.T_VIDEO -> {
                        val f = Proto.parseVideo(b, len) ?: return
                        reasm.push(nowMs, f, b, len)
                    }
                    Proto.T_AUDIO, Proto.T_AUDIO_RED -> {
                        val pl = audio ?: return
                        val rx = arx ?: return
                        audioOut.clear()
                        if (Proto.type(b) == Proto.T_AUDIO) {
                            val a = Proto.parseAudio(b, len) ?: return
                            rx.onPlain(a.seq, a.tsMs.toLong() and 0xFFFFFFFFL, b.copyOfRange(a.payloadOffset, len), audioOut)
                        } else {
                            val a = Proto.parseAudioRed(b, len) ?: return
                            val prev = if (a.prevLen > 0) b.copyOfRange(a.prevOffset, a.prevOffset + a.prevLen) else null
                            rx.onRed(a.seq, a.tsMs.toLong() and 0xFFFFFFFFL, prev, b.copyOfRange(a.curOffset, a.curOffset + a.curLen), audioOut)
                        }
                        // The last chunk is the one that just arrived; a rebuilt older one (if any) comes first.
                        for (c in audioOut) {
                            val t = Math.round(c.tsMs).toLong() and 0xFFFFFFFFL
                            if (cur.start.clockSync != 0 && c === audioOut.last()) clock?.arrival?.onTimestamp(t, nowMs)
                            pl.push(t, c.data)
                        }
                    }
                    Proto.T_AUDIO_OPUS -> {
                        val dec = opusDec ?: return
                        val rx = opusRx ?: return
                        val a = Proto.parseAudioOpus(b, len) ?: return
                        val ts = a.tsMs.toLong() and 0xFFFFFFFFL
                        opusOut.clear()
                        rx.onDatagram(a.seq, ts, a.frames, opusOut)
                        if (cur.start.clockSync != 0 && opusOut.isNotEmpty()) clock?.arrival?.onTimestamp(ts, nowMs)
                        for (f in opusOut) dec.submit(f.tsMs, f.data)
                    }
                    Proto.T_PING -> send(sock, b.copyOf(len).also { it[1] = Proto.T_PONG.toByte() }, cur.peer)
                    Proto.T_BYE -> endSession(sendBye = false)
                }
            }
        }
    }

    private fun beginSession(sock: DatagramSocket, id: Int, peer: InetSocketAddress, s: Proto.Start) {
        endSession(sendBye = false)
        val surf = surface
        if (surf == null || !surf.isValid || s.vcodec != Proto.VCODEC_H264) return // sender will retry START
        // Lip-sync needs the sender's shared audio/video clock (v0.2+); an older sender gets "show at once".
        val synced = s.clockSync != 0
        val mc = if (synced) MediaClock { audio?.clockMs() } else null
        clock = mc
        arx = AudioRx(s.audioRate.coerceAtLeast(1), s.audioChannels.coerceAtLeast(1))
        // Only what the sender offered AND we can do; an older sender offers nothing and gets the v0.3 behaviour.
        var features = s.features and (Proto.FEAT_NACK or Proto.FEAT_AUDIO_RED or Proto.FEAT_OPUS)
        // Opus needs 48 kHz stereo, a codec that really decoded our test tone, and a working audio player to feed.
        var opus = false
        if (features and Proto.FEAT_OPUS != 0 && s.audioRate == 48000 && s.audioChannels == 2 && OpusDecoder.selfTest()) {
            try {
                val rx = OpusRx()
                opusDec = OpusDecoder { ts, pcm -> audio?.push(ts, pcm) }
                opusRx = rx
                opus = true
            } catch (e: Throwable) { Log.w(TAG, "opus decoder: $e") }
        }
        if (!opus) features = features and Proto.FEAT_OPUS.inv() // the sender then stays with PCM + redundancy
        reasm = Reassembler(this, if (features and Proto.FEAT_NACK != 0) Reassembler.Config() else Reassembler.Config.withoutNack())
        try {
            decoder = VideoDecoder(surf, s.width, s.height, s.fps, { needKey = true }, mc,
                if (mc != null) SyncController { ms -> audio?.delayBy(ms) } else null)
        } catch (e: Throwable) {
            Log.e(TAG, "decoder: $e")
            listener.onError("Не удалось запустить видеодекодер: $e")
            return
        }
        if (s.audioRate in 8000..96000 && s.audioChannels in 1..2) {
            try { audio = AudioPlayer(s.audioRate, s.audioChannels) } catch (e: Throwable) { Log.w(TAG, "audio: $e") }
        }
        session = Session(id, peer, s, features)
        reasm.reset()
        keyPolicy.onKey(); keyPolicy.need()
        awaitingKey = true
        needKey = true
        lastPacketAt = System.nanoTime()
        lastReport = lastPacketAt
        recvFrames = 0; lastRecvFrames = 0; lastShown = 0
        lastDecoded = 0; lastDropped = 0; lastLost = 0; lastNacked = 0; lastRecovered = 0
        send(sock, Proto.startAck(id, features), peer)
        listener.onStreaming(true, "${peer.address.hostAddress}  ${s.width}x${s.height} ${s.fps} к/с", s.width, s.height)
    }

    private fun endSession(sendBye: Boolean) {
        val s = session ?: return
        session = null
        if (sendBye) socket?.let { send(it, Proto.simple(Proto.T_BYE, s.id), s.peer) }
        decoder?.release(); decoder = null
        opusDec?.release(); opusDec = null; opusRx = null
        audio?.release(); audio = null
        clock = null
        reasm.reset()
        listener.onStreaming(false, "")
    }

    private fun housekeeping(sock: DatagramSocket) {
        while (true) { (tasks.poll() ?: break)() }
        val now = System.nanoTime()
        val s = session
        if (s != null) {
            if ((now - lastPacketAt) / 1_000_000 > SESSION_TIMEOUT_MS) { endSession(sendBye = false); return }
            val nowMs = now / 1_000_000
            // Ask for exactly the missing packets; a frame that outlives the latency budget is given up (-> keyframe).
            nackOut.clear()
            reasm.poll(nowMs, nackOut)
            var from = 0
            while (from < nackOut.size) {
                val to = minOf(from + Proto.MAX_NACK_ITEMS, nackOut.size)
                send(sock, Proto.nack(s.id, nackOut.subList(from, to)), s.peer)
                from = to
            }
            if (needKey) keyPolicy.need() // the decoder asked for one
            if (keyPolicy.poll(nowMs, reasm.keyPending())) send(sock, Proto.simple(Proto.T_KEYREQ, s.id), s.peer)
            if ((now - lastReport) / 1_000_000 >= 1000) {
                lastReport = now
                val d = decoder
                val dec = d?.decoded?.get() ?: 0
                val drp = d?.dropped?.get() ?: 0
                val lost = reasm.lost
                val nk = reasm.nackedPkts
                val rc = reasm.recoveredPkts
                val shown = d?.shown?.get() ?: 0
                val rcv = recvFrames
                send(sock, Proto.report(s.id, Proto.Report(dec - lastDecoded, lost - lastLost, drp - lastDropped,
                    nk - lastNacked, rc - lastRecovered, rcv - lastRecvFrames, shown - lastShown)), s.peer)
                val ap = audio
                val sound = when {
                    ap == null -> "нет"
                    opusDec != null -> "Opus"
                    else -> "PCM"
                }
                listener.onStats("принято ${rcv - lastRecvFrames} · показано ${shown - lastShown} к/с · декодировано ${dec - lastDecoded}" +
                    " · потеряно кадров ${lost - lastLost}, пропущено ${drp - lastDropped}" +
                    " · перезапросов $nk, вернулось $rc" +
                    " · звук $sound" + (if (ap != null) ", буфер ${ap.bufferedMs()}/${ap.startMs} мс, перебоев ${ap.underruns.get()}" else ""))
                lastDecoded = dec; lastDropped = drp; lastLost = lost; lastNacked = nk; lastRecovered = rc
                lastRecvFrames = rcv; lastShown = shown
            }
        }
        if ((now - lastHello) / 1_000_000 >= 1000) {
            lastHello = now
            announce(sock)
        }
    }

    // ---- Reassembler.Sink (called on the network thread)
    override fun onFrame(frameId: Int, tsMs: Int, key: Boolean, data: ByteArray) {
        val d = decoder ?: return
        recvFrames++
        clock?.arrival?.onTimestamp(tsMs.toLong() and 0xFFFFFFFFL, System.nanoTime() / 1_000_000)
        if (awaitingKey) {
            if (!key) return
            awaitingKey = false
        }
        if (key) { needKey = false; keyPolicy.onKey() }
        if (!d.submit(data, tsMs)) { awaitingKey = true; needKey = true; keyPolicy.need() }
    }

    override fun onLost() {
        awaitingKey = true
        needKey = true
        keyPolicy.need()
    }

    // ---- discovery
    private fun helloBytes(): ByteArray =
        Proto.hello(Proto.Hello(Proto.PORT, caps.w, caps.h, caps.fps, session != null, deviceName))

    private fun announce(sock: DatagramSocket) {
        val data = helloBytes()
        val targets = HashSet<InetAddress>()
        try { targets.add(InetAddress.getByName("255.255.255.255")) } catch (_: Throwable) {}
        for (a in broadcastAddresses()) targets.add(a)
        for (t in targets) send(sock, data, InetSocketAddress(t, Proto.DISCOVERY_PORT))
    }

    private fun detectCaps(): Caps {
        if (Build.VERSION.SDK_INT < 21) return Caps(1280, 720, 30) // no codec list API before Android 5: be modest
        try {
            val infos = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { !it.isEncoder && it.supportedTypes.any { t -> t.equals("video/avc", true) } }
            val hw = infos.filter { !it.name.startsWith("OMX.google") && !it.name.startsWith("c2.android") }
            val pool = if (hw.isNotEmpty()) hw else infos
            var best = Caps(1280, 720, 30)
            for (info in pool) {
                val v = info.getCapabilitiesForType("video/avc").videoCapabilities ?: continue
                val soft = hw.isEmpty()
                val sizes = if (soft) listOf(1280 to 720) else listOf(1920 to 1080, 1280 to 720)
                for ((w, h) in sizes) {
                    for (f in listOf(60, 30, 25)) {
                        if (v.areSizeAndRateSupported(w, h, f.toDouble())) {
                            if (w * h > best.w * best.h || (w == best.w && f > best.fps)) best = Caps(w, h, f)
                            break
                        }
                    }
                }
            }
            return best
        } catch (e: Throwable) {
            Log.w(TAG, "caps: $e")
            return Caps(1920, 1080, 30)
        }
    }

    companion object {
        private const val TAG = "SkpEngine"
        private const val SESSION_TIMEOUT_MS = 4000L

        fun broadcastAddresses(): List<InetAddress> {
            val out = ArrayList<InetAddress>()
            try {
                for (ni in NetworkInterface.getNetworkInterfaces()) {
                    if (!ni.isUp || ni.isLoopback) continue
                    for (ia in ni.interfaceAddresses) {
                        val b = ia.broadcast
                        if (b != null && ia.address is Inet4Address) out.add(b)
                    }
                }
            } catch (_: Throwable) {}
            return out
        }

        /** Local IPv4 addresses (for showing on screen as a manual-connect fallback). */
        fun localAddresses(): List<String> {
            val out = ArrayList<String>()
            try {
                for (ni in NetworkInterface.getNetworkInterfaces()) {
                    if (!ni.isUp || ni.isLoopback) continue
                    for (ia in ni.interfaceAddresses) {
                        val a = ia.address
                        if (a is Inet4Address && !a.isLinkLocalAddress) out.add(a.hostAddress ?: continue)
                    }
                }
            } catch (_: Throwable) {}
            return out
        }
    }
}
