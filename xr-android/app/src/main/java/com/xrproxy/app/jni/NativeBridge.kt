package com.xrproxy.app.jni

import com.xrproxy.app.service.XrVpnService

/**
 * JNI bridge to the Rust xr-core VPN engine.
 */
object NativeBridge {
    init {
        System.loadLibrary("xr_proxy")
    }

    /**
     * Live reference to the running XrVpnService, updated in the service
     * lifecycle (onCreate/onDestroy). Used only by the Rust-side callback
     * below, so `protectSocket` always goes through whichever service is
     * currently alive — avoids stale references after Activity recreation.
     */
    @Volatile
    var current: XrVpnService? = null

    /**
     * Called FROM Rust (via JNI callback) to protect a socket fd.
     * Protected sockets bypass the VPN tunnel — critical to avoid routing loops.
     */
    @JvmStatic
    fun protectSocket(fd: Int): Boolean {
        return current?.protect(fd) ?: false
    }

    /** Start the VPN engine. Returns null on success, or an error message on failure. */
    external fun nativeStart(tunFd: Int, configJson: String): String?
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
