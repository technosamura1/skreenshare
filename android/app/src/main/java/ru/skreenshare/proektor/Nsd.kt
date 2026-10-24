package ru.skreenshare.proektor

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

/** Advertises the receiver over mDNS / DNS-SD so the PC can find it without any typing. */
class Nsd(ctx: Context, private val name: String) {
    private val mgr = ctx.getSystemService(Context.NSD_SERVICE) as? NsdManager
    private var listener: NsdManager.RegistrationListener? = null

    fun register() {
        val m = mgr ?: return
        val info = NsdServiceInfo().apply {
            serviceName = name
            serviceType = Proto.MDNS_TYPE
            port = Proto.PORT
        }
        val l = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(i: NsdServiceInfo) { Log.i(TAG, "NSD registered as ${i.serviceName}") }
            override fun onRegistrationFailed(i: NsdServiceInfo, code: Int) { Log.w(TAG, "NSD register failed $code") }
            override fun onServiceUnregistered(i: NsdServiceInfo) {}
            override fun onUnregistrationFailed(i: NsdServiceInfo, code: Int) {}
        }
        try {
            m.registerService(info, NsdManager.PROTOCOL_DNS_SD, l)
            listener = l
        } catch (e: Throwable) {
            Log.w(TAG, "NSD: $e")
        }
    }

    fun unregister() {
        val l = listener ?: return
        listener = null
        try { mgr?.unregisterService(l) } catch (_: Throwable) {}
    }

    companion object { private const val TAG = "SkpNsd" }
}
