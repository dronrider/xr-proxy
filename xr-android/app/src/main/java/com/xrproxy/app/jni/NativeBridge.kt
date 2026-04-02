package com.xrproxy.app.jni

/**
 * JNI bridge to the Rust xr-core VPN engine.
 */
object NativeBridge {
    init {
        System.loadLibrary("xr_proxy")
    }

    /** Start the VPN engine. Returns 0 on success, negative on error. */
    external fun nativeStart(tunFd: Int, configJson: String): Int

    /** Stop the VPN engine. */
    external fun nativeStop()

    /** Get current VPN state as string. */
    external fun nativeGetState(): String

    /** Get stats as JSON string. */
    external fun nativeGetStats(): String

    /** Push a raw IP packet from TUN into the engine. */
    external fun nativePushPacket(packet: ByteArray)

    /** Pop an outbound packet from the engine. Returns null if none. */
    external fun nativePopPacket(): ByteArray?
}
