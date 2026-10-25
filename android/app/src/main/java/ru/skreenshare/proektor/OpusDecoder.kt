package ru.skreenshare.proektor

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Process
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicInteger

/**
 * Opus -> 16-bit PCM through the platform's MediaCodec ("audio/opus", Android 5.0+). Input is the Opus frames of the
 * sender (10 ms, 48 kHz stereo); every decoded buffer is handed to [onPcm] with the timestamp of the packet it came from.
 *
 * Many cheap Android boxes ship a broken or missing Opus codec, so Opus is only offered to the sender after
 * [selfTest] has really decoded a known tone on this device. If anything fails, the sender keeps using PCM.
 */
class OpusDecoder(private val onPcm: (tsMs: Long, pcm: ByteArray) -> Unit) {
    private val codec: MediaCodec
    @Volatile private var running = true
    private val drainer: Thread
    val dropped = AtomicInteger()

    init {
        codec = MediaCodec.createDecoderByType(MIME)
        try {
            codec.configure(format(), null, null, 0)
            codec.start()
        } catch (e: Throwable) {
            try { codec.release() } catch (_: Throwable) {}
            throw e
        }
        drainer = Thread({ drainLoop() }, "opus-drain").also { it.start() }
    }

    /** One Opus frame. Never blocks: if the codec has no free input buffer the frame is dropped (and concealed later). */
    fun submit(tsMs: Long, data: ByteArray) {
        try {
            val idx = codec.dequeueInputBuffer(0)
            if (idx < 0) { dropped.incrementAndGet(); return }
            val buf = inputBuffer(idx)
            buf.clear()
            buf.put(data)
            codec.queueInputBuffer(idx, 0, data.size, tsMs * 1000, 0)
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "opus in: $e")
        }
    }

    @Suppress("DEPRECATION")
    private fun inputBuffer(idx: Int): ByteBuffer =
        if (Build.VERSION.SDK_INT >= 21) codec.getInputBuffer(idx)!! else codec.inputBuffers[idx]

    @Suppress("DEPRECATION")
    private fun outputBuffer(idx: Int): ByteBuffer =
        if (Build.VERSION.SDK_INT >= 21) codec.getOutputBuffer(idx)!! else codec.outputBuffers[idx]

    private fun drainLoop() {
        try { Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO) } catch (_: Throwable) {}
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                val idx = codec.dequeueOutputBuffer(info, 10_000)
                if (idx >= 0) {
                    if (info.size > 0) {
                        val out = outputBuffer(idx)
                        out.position(info.offset)
                        out.limit(info.offset + info.size)
                        val pcm = ByteArray(info.size)
                        out.get(pcm)
                        onPcm(info.presentationTimeUs / 1000, pcm)
                    }
                    codec.releaseOutputBuffer(idx, false)
                }
            }
        } catch (e: Throwable) {
            if (running) Log.w(TAG, "opus out: $e")
        }
    }

    fun release() {
        running = false
        try { drainer.join(300) } catch (_: Throwable) {}
        try { codec.stop() } catch (_: Throwable) {}
        try { codec.release() } catch (_: Throwable) {}
    }

    companion object {
        private const val TAG = "SkpOpus"
        const val MIME = "audio/opus"

        /** "OpusHead" identification header (RFC 7845): 1 version, 2 channels, pre-skip 0, 48000 Hz, gain 0, mapping 0. */
        fun opusHead(): ByteArray {
            val b = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
            b.put("OpusHead".toByteArray(Charsets.US_ASCII)).put(1).put(2).putShort(0).putInt(48000).putShort(0).put(0)
            return b.array()
        }

        private fun format(): MediaFormat {
            val f = MediaFormat.createAudioFormat(MIME, 48000, 2)
            f.setByteBuffer("csd-0", ByteBuffer.wrap(opusHead()))
            // codec delay (pre-skip) and seek pre-roll in ns, 64-bit little endian: both 0, our frames start at once
            f.setByteBuffer("csd-1", ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(0).also { it.flip() })
            f.setByteBuffer("csd-2", ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(0).also { it.flip() })
            f.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 1275)
            return f
        }

        /** 16 real Opus frames (a 440 Hz tone, 10 ms each, made by the same libopus and settings as the sender's). */
        private val TONE_HEX = listOf(
        "f4b53f0efb1eb00d99295ac9cee9f5c5b4ee1c8149713f7b77d0381e74a9316de7d1b7baaeba3f2c0b0dff1dddc2ba55c79acaa1369800a32a2abd00381cd1361b4dd371f5154ad5bde1351b4b6c2ea1d067f828873e88873e8ef37aa44aae591d4a8f27bdffd0231f30440a2d710cfa53c5c23814cc4dd8f5bb981756cf89ec84e4a31f0ba431048d9eb5af79e08dbe84a951e96a4dffe1eb4b4a07ff4a012c",
        "f4b2f55b9abd35640d9c66d65f6e8b33234b9415ce69b96caa788b3e4b5a88bab3de595e72c67d2547ca9fd6189ff754558f0f7bc37a30b882b6e6db374eae3f6ecf80ad07182e799c8ed7a063a0b399db42878c74da0dc3dac4aa478daa66374aed904e4b0aa8b52cf06309916ba5dbcdec833182a79eaa16694457016732044615d5396fdc7ba98634383594394d58006ba180dff6837fe057ff8be15eabae",
        "f4ad7d706b7b7bbda7be89290e838218cc6366fb7a9b743ded05014b305ec532144815957ca89b17606b65218a1ab0499a49932553dc3cc95c8c2e22544fc73267b8f52c149a7a815e6f3e9a0842fb1830702dc1e93bfeec66f68571853c22a1df871fa898926f64c5d68301845035b2c00e02b26ab22c40889754a1c07e42bc059b40a1a9c5bdcaea14a07198783e1d98f92b9fc8912da800a1ebad0036bfae",
        "f4ad27ae5bdff0f7df6879a92305a5b835d5abe5f8d7d5efcc1ac665b5e3e4dd5b48c30749a7c1f800ac314f7fb244299dfb9642e9a1c9d1949d84a67514903a44c7718e93f3814b5586a825e96221d374530518cea1f16a4f66c05c1bbb629f77b1bddc330961f79364e816c4492e2f184ca89833533d166867c8892e1e40b4f48736b10ce83fe8b36df05dcaf0b6cc0de492287a802ad07ffbb5491cf5ddae",
        "f4ad7d3b5131dea9517cdac4089e18454eb494e83caa24b5a17836783c4c40a78bde6f2d2514eeca7755122b0a38f0d1be77504906a274811b8b9bec13519bd05ba628e83680ee39a47b324c8950ed0393e1cd77535e5dd1ac9512d31d67044eb13141d84b1a0830d1eb13626965de5fbc202a3e5d5aa9ed18ff74cd9eed26760c33422acc266e6c5569268c9ea7955fc916b615e1fff40bf22adddb085b99ae",
        "f4ad7d3bb892f22dc7509d135c1e3b1a2cd2168fa1063688871e730d906b5f8ae1255e47cfb9da0a2ff052e47cd706b9b3ec80dced3ec9d48885041f87db220266c9d588af3aea849227fe42eed0e3b79802d57bfdcd988c6e11163d90063c41033de88c26c629136a5669c8272f1475335da83ad971ea5d0dcb1ed57f9b7c879581cc0ce76b1596c0a40054141adf378816b6a1400be1e0a07feeb6c67f77ae",
        "f4aab64da6f866feff0be123787cac5407d858612fa66a4e657f891a232d02f49748116fe2ee84da490e8c79abc6da6268bdc4f480ee4ee982536b27eb5f7b481b79392e88a9fa8917c49359eef6c610d5a4e08ca772a72ec8f94046fff182d07d300fbd29ef04c25a8bbc3abcabd6975db0990b67b6c91ef323b5e56f6807c49fa9268b1304ae80ad36558c498ca8a0de484ffd2805782d1200ee00f7fe23ae",
        "f4aab6bc67c384591cf2f2428c68ff839e9cbfe382a31f6e0295455eda1fc8138a0266e363839eb678bc33e63e6b8f94ddec146590c4f442395d40e04100a561e7ba531b080ed1110f647f12ab1bfb0cc93abcc01abf4c975a8b27442b95043e6c2f0042dbefab02b926f17e81870f07fd5983da7467df02d816792e052bbfc15583ab2ef7639cc5290f7804322634d20de6d6a852802d285008932dd29445ae",
        "f4ad27ae5bdff0f7df6879a92305a5b835d5abe5f60f0ba0772888a9b763d2dd1d17ea61ad0d9795ed577c131223d865125bb23db3d15ff766fd427482c4a6a7295ca6d13f48cdf869a7f199caff906cfb52c0c42bccc58295f5f1882e0dddb14e2ee01b421084b0fbc9b1425df4495c3f7244cb75908afba8b09ef866857ca3ae1d17835ae66e1eb2f66ec6d6e0cd0c0de48da87d002ad07ffdd544a115ddae",
        "f4ad7d3b5131dea9517cdac69ec78795e90e4b6c0b739959a35afc174e19b4685cae17bc2463e78a6e88db0c97c0d2760fe63f19dd3ae2d9c07f84384a16df564ce2d297276946863325a94d563836728d3c5aac720f9391cc1d1acf878571412c905be0c347af1a0d84b4b2e26c4797ef080a88fd6214f68c7fba66cf76933b0619a115661337362a8304ec9ea7935fc916b615e1fff40bf22adddb085b77ae",
        "f4ad7d3bb892f22dc750a7f9d2cdb3765ce3b9fbc43ccb0225ab275784881a21c14cd219afad479e1341d4a6b350318a3df425d8cab58887dbab7207f8b1a5ff1e0c78c5117df8f9dfd64c7454443817e478376a4fa00c9a6e3133c082018e2db61fe944613625225e92482a73978a3a99aed41d6cb8f48486e58f6abfcdbe43cac0e606726af356c0a40054141adf57881b48a1400be1e000776636c67f99ae",
        "f4aab64da6f866feff0be17978b63e63fdf6a7f24c9651aeccd2dbc8c8af68e5508e45d0925409458babab3161c2d55f5ad55d368c61ca075ff0cf756821f631f7abc5a5d37b35c1510a7f000811308c9a2e19ef7391016f784a29c9608160b0617fe8afbe60557d29ef043abcabd6975db0990b67b6c91ef32586156f645f193f52a7684304ae8c2d37958c498ca8a0de4807fd2805782f9200ee00f7fe23ae",
        "f4aab6bc67c384591cf44548191d76e0572089ac260e06a92ba9539dceca4ceedf9048697a9719707e85ac274429d83f5ab3b6ec564002c67ba95f9fcb644814ec507273ae2f366d1415346568ef8a6d37c9aee875cbe05d1762b229e270f9a5440bc010b4ae2b02b926f6c7a061c3cd51e983da7467df02d816790dd2577f82ab07565df54c0614a43de010c898d348379b5aa14a00b4a0a02accb74a4845ae",
        "f4ad27ae5bdff0f7df687a46de62d233cd035c177663b4b6fcc7fa2350e2895aa950b86441612c15a1a77154103b7beb3aa1b266777b6c9f5e7d482f956ef5fa9067f7eca1d9b6613be9226e7c9be7444637397ebc5717bab6bf2ac0d24123b45b06eec6f78355a1ca5f057ef7366b1ee2f81e0634b8d1363807c888f95e40ad748736b10cd69396b36df05dd6e1aa0c0de5ada87a802ad07ffdd544a115ddae",
        "f4ad7d3b5131dea9517cdaff0e022f04de4ad103bed795b7bf097f217057cf999a9e666edca9048f59533c0106be3ab7337623771cfe8b3f55c1a96c91bd1ca94eb4f81e488100930d65547e9943de4cf31fc3eb038c9dff6ccb09a9baf593a69fee02f5f98ffd682cf413a1c96e85a10ffa6c3ae2935257b6e0f98f98cd0e130619f5ba43ce26c52a8308164f05825fc916b61541fff40bf22adddb086d77ae",
        "f4ad7d3bb892f22dc750a7f9d2cdb3765ce3b9fbc3ae1876a002ac0c0e1248d556d48349bb63fcf76e9cdfa7481fc4d02db99d20192145315ccabdbd9f01cc444ce3e91304ab34920b4fe1c74b97467743304be2a3fc559a148a61217b61fe98e2b14344613625225e9254e413978a3a99aed41d6cb8f48486e58f6abfcdbe43cac0e606726af356c0a40054141adf378816c8a1e00be1e000776eb6c67f99ae"
        )

        private fun unhex(s: String) = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }

        @Volatile private var tested: Boolean? = null

        /**
         * Does this device decode Opus? Feeds the tone above and expects audible PCM (peak above 1000, at least 60 ms)
         * within a second. Cached; never throws.
         */
        fun selfTest(): Boolean {
            tested?.let { return it }
            val ok = try { runSelfTest() } catch (e: Throwable) { Log.w(TAG, "self-test: $e"); false }
            Log.i(TAG, "opus self-test: $ok")
            tested = ok
            return ok
        }

        private fun runSelfTest(): Boolean {
            if (Build.VERSION.SDK_INT < 21) return false
            var peak = 0
            var bytes = 0
            val lock = Any()
            val dec = OpusDecoder { _, pcm ->
                var p = 0
                var i = 0
                while (i + 1 < pcm.size) {
                    val v = ((pcm[i].toInt() and 0xFF) or (pcm[i + 1].toInt() shl 8)).toShort().toInt()
                    p = maxOf(p, Math.abs(v))
                    i += 2
                }
                synchronized(lock) { peak = maxOf(peak, p); bytes += pcm.size }
            }
            try {
                val t0 = System.nanoTime()
                for ((n, h) in TONE_HEX.withIndex()) {
                    dec.submit(n * 10L, unhex(h))
                    Thread.sleep(2)
                }
                while ((System.nanoTime() - t0) / 1_000_000 < 1000) {
                    synchronized(lock) { if (bytes >= 48 * 60 * 4 && peak > 1000) return true }
                    Thread.sleep(10)
                }
                return false
            } finally {
                dec.release()
            }
        }
    }
}
