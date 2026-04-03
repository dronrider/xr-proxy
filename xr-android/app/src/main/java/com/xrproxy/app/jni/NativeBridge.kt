package com.xrproxy.app.jni

import android.net.VpnService

/**
 * JNI bridge to the Rust xr-core VPN engine.
 */
object NativeBridge {
    init {
        System.loadLibrary("xr_proxy")
    }

    /** Reference to the active VpnService (set before nativeStart). */
    @Volatile
    var vpnService: VpnService? = null

    /**
     * Called FROM Rust (via JNI callback) to protect a socket fd.
     * Protected sockets bypass the VPN tunnel — critical to avoid routing loops.
     */
    @JvmStatic
    fun protectSocket(fd: Int): Boolean {
        return vpnService?.protect(fd) ?: false
    }

    /** Start the VPN engine. Returns 0 on success, negative on error. */
    external fun nativeStart(tunFd: Int, configJson: String): Int
    external fun nativeStop()
    external fun nativeGetState(): String
    external fun nativeGetStats(): String
    /** Get full error log (newline-separated). */
    external fun nativeGetErrorLog(): String

    /** Clear error log. */
    external fun nativeClearErrorLog()

    external fun nativePushPacket(packet: ByteArray)
    external fun nativePopPacket(): ByteArray?
}
