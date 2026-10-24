package ru.skreenshare.proektor

import android.app.Activity
import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.view.KeyEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView

/** One full-screen SurfaceView plus a "waiting for the computer" screen on top of it. Back exits. */
class MainActivity : Activity(), Engine.Listener, SurfaceHolder.Callback {
    private lateinit var surfaceView: SurfaceView
    private lateinit var overlay: View
    private lateinit var title: TextView
    private lateinit var hint: TextView
    private lateinit var addr: TextView
    private lateinit var engine: Engine
    private lateinit var nsd: Nsd
    private var multicast: WifiManager.MulticastLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var streaming = false

    @Suppress("DEPRECATION")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON or WindowManager.LayoutParams.FLAG_FULLSCREEN)
        window.setBackgroundDrawable(null)

        val root = FrameLayout(this)
        root.setBackgroundColor(Color.BLACK)
        surfaceView = SurfaceView(this)
        surfaceView.holder.addCallback(this)
        root.addView(surfaceView, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))

        val box = LinearLayout(this)
        box.orientation = LinearLayout.VERTICAL
        box.gravity = Gravity.CENTER
        box.setBackgroundColor(0xFF101218.toInt())
        title = text(36f, Color.WHITE, true)
        hint = text(20f, 0xFFB8BDCC.toInt(), false)
        addr = text(20f, 0xFF8FA8FF.toInt(), false)
        val back = text(16f, 0xFF7C8196.toInt(), false)
        back.text = "Кнопка «Назад» на пульте — выход"
        title.text = getString(R.string.waiting)
        hint.text = getString(R.string.hint)
        box.addView(title); box.addView(hint); box.addView(addr); box.addView(back)
        overlay = box
        root.addView(box, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        setContentView(root)
        // Rotation / split screen on a phone: re-fit the picture (letterboxed) to the new size.
        root.addOnLayoutChangeListener { _, l, tp, r, b, ol, ot, or, ob ->
            if (streaming && (r - l != or - ol || b - tp != ob - ot)) fitSurface(picW, picH)
        }
        hideSystemUi()

        engine = Engine(this, this)
        nsd = Nsd(this, "Skreenshare " + engine.deviceName)
    }

    private fun text(sp: Float, color: Int, bold: Boolean) = TextView(this).apply {
        textSize = sp
        setTextColor(color)
        gravity = Gravity.CENTER
        setPadding(40, 14, 40, 14)
        if (bold) typeface = Typeface.DEFAULT_BOLD
    }

    @Suppress("DEPRECATION")
    private fun hideSystemUi() {
        window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_LAYOUT_STABLE or
            View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
            View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or View.SYSTEM_UI_FLAG_FULLSCREEN or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemUi()
    }

    @Suppress("DEPRECATION")
    override fun onStart() {
        super.onStart()
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        try {
            multicast = wifi?.createMulticastLock("skreenshare")?.also { it.setReferenceCounted(false); it.acquire() }
            val mode = if (Build.VERSION.SDK_INT >= 29) 4 /* WIFI_MODE_FULL_LOW_LATENCY */ else WifiManager.WIFI_MODE_FULL_HIGH_PERF
            wifiLock = wifi?.createWifiLock(mode, "skreenshare")?.also { it.setReferenceCounted(false); it.acquire() }
        } catch (_: Throwable) {}
        updateAddress()
        engine.start()
        nsd.register()
    }

    override fun onStop() {
        nsd.unregister()
        engine.stop()
        try { wifiLock?.release() } catch (_: Throwable) {}
        try { multicast?.release() } catch (_: Throwable) {}
        showStreaming(false, "")
        super.onStop()
    }

    private fun updateAddress() {
        val ips = Engine.localAddresses()
        addr.text = if (ips.isEmpty()) getString(R.string.no_network)
        else getString(R.string.address, engine.deviceName, ips.joinToString(", "))
    }

    // ---- Engine.Listener (network thread)
    override fun onStreaming(active: Boolean, info: String, width: Int, height: Int) {
        runOnUiThread { showStreaming(active, info, width, height) }
    }

    override fun onError(msg: String) {
        runOnUiThread { title.text = getString(R.string.error); hint.text = msg; overlay.visibility = View.VISIBLE }
    }

    /** Keeps the picture's aspect ratio (black bars) instead of stretching it over the whole screen. */
    private var picW = 0
    private var picH = 0

    private fun fitSurface(w: Int, h: Int) {
        picW = w; picH = h
        val root = surfaceView.parent as? View ?: return
        val rw = root.width
        val rh = root.height
        val lp = FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT, Gravity.CENTER)
        if (w > 0 && h > 0 && rw > 0 && rh > 0) {
            if (w.toLong() * rh > h.toLong() * rw) { lp.width = rw; lp.height = (rw.toLong() * h / w).toInt() }
            else { lp.height = rh; lp.width = (rh.toLong() * w / h).toInt() }
        }
        surfaceView.layoutParams = lp
    }

    private fun showStreaming(active: Boolean, info: String, w: Int = 0, h: Int = 0) {
        streaming = active
        if (active) fitSurface(w, h)
        overlay.visibility = if (active) View.GONE else View.VISIBLE
        if (!active) {
            title.text = getString(R.string.waiting)
            hint.text = getString(R.string.hint)
            updateAddress()
        }
    }

    // ---- SurfaceHolder.Callback
    override fun surfaceCreated(holder: SurfaceHolder) { engine.setSurface(holder.surface) }
    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
    override fun surfaceDestroyed(holder: SurfaceHolder) { engine.setSurface(null) }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK) {
            finish()
            return true
        }
        return super.onKeyDown(keyCode, event)
    }
}
