package ru.skreenshare.proektor

import android.content.Context
import android.media.MediaCodecList
import android.os.Build
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
    }

    private class Session(val id: Int, val peer: InetSocketAddress, val start: Proto.Start)

    @Volatile private var running = false
    @Volatile private var surface: Surface? = null
    private var socket: DatagramSocket? = null
    private var thread: Thread? = null

    // Only touched from the network thread (except where marked).
    private var session: Session? = null
    private var decoder: VideoDecoder? = null
    private var audio: AudioPlayer? = null
    private val reasm = Reassembler(this)
    private var awaitingKey = true
    private var lastKeyReq = 0L
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
        thread = Thread({ run() }, "net").also { it.start() }
    }

    fun stop() {
        running = false
        try { socket?.close() } catch (_: Throwable) {}
        try { thread?.join(1000) } catch (_: Throwable) {}
        thread = null
    }

    private fun run() {
        val sock = try {
            DatagramSocket(null).apply {
                reuseAddress = true
                bind(InetSocketAddress(Proto.PORT))
                broadcast = true
                try { receiveBufferSize = 4 shl 20 } catch (_: Throwable) {}
                soTimeout = 20
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
                    send(sock, Proto.simple(Proto.T_START_ACK, sid), peer) // sender retries until it hears this
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
                        reasm.push(f, b, len)
                    }
                    Proto.T_AUDIO -> {
                        val a = Proto.parseAudio(b, len) ?: return
                        audio?.push(b.copyOfRange(a.payloadOffset, len))
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
        try {
            decoder = VideoDecoder(surf, s.width, s.height, s.fps) { needKey = true }
        } catch (e: Throwable) {
            Log.e(TAG, "decoder: $e")
            listener.onError("Не удалось запустить видеодекодер: $e")
            return
        }
        if (s.audioRate in 8000..96000 && s.audioChannels in 1..2) {
            try { audio = AudioPlayer(s.audioRate, s.audioChannels) } catch (e: Throwable) { Log.w(TAG, "audio: $e") }
        }
        session = Session(id, peer, s)
        reasm.reset()
        awaitingKey = true
        needKey = true
        lastPacketAt = System.nanoTime()
        lastReport = lastPacketAt
        lastDecoded = 0; lastDropped = 0; lastLost = 0
        send(sock, Proto.simple(Proto.T_START_ACK, id), peer)
        listener.onStreaming(true, "${peer.address.hostAddress}  ${s.width}x${s.height} ${s.fps} к/с", s.width, s.height)
    }

    private fun endSession(sendBye: Boolean) {
        val s = session ?: return
        session = null
        if (sendBye) socket?.let { send(it, Proto.simple(Proto.T_BYE, s.id), s.peer) }
        decoder?.release(); decoder = null
        audio?.release(); audio = null
        reasm.reset()
        listener.onStreaming(false, "")
    }

    private fun housekeeping(sock: DatagramSocket) {
        while (true) { (tasks.poll() ?: break)() }
        val now = System.nanoTime()
        val s = session
        if (s != null) {
            if ((now - lastPacketAt) / 1_000_000 > SESSION_TIMEOUT_MS) { endSession(sendBye = false); return }
            // A frame that has been waiting for its last fragments for too long is gone.
            if (reasm.oldestOlderThan(100)) reasm.giveUpOldest()
            if (needKey && (now - lastKeyReq) / 1_000_000 > 150) {
                lastKeyReq = now
                send(sock, Proto.simple(Proto.T_KEYREQ, s.id), s.peer)
            }
            if ((now - lastReport) / 1_000_000 >= 1000) {
                lastReport = now
                val d = decoder
                val dec = d?.decoded?.get() ?: 0
                val drp = d?.dropped?.get() ?: 0
                val lost = reasm.lost
                send(sock, Proto.report(s.id, Proto.Report(dec - lastDecoded, lost - lastLost, drp - lastDropped)), s.peer)
                lastDecoded = dec; lastDropped = drp; lastLost = lost
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
        if (awaitingKey) {
            if (!key) return
            awaitingKey = false
            needKey = false
        }
        if (!d.submit(data, tsMs)) { awaitingKey = true; needKey = true }
    }

    override fun onLost() {
        awaitingKey = true
        needKey = true
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
