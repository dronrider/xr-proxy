package com.xrproxy.app.jni

import android.net.Network
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
     * Underlying non-VPN network captured by `XrVpnService` via
     * `ConnectivityManager` before and during the VPN session. The VPN
     * service updates it from a `NetworkCallback` and nulls it on stop.
     *
     * Used by `resolveDomain` below to bypass the VPN tunnel when resolving
     * hostnames for direct-mode traffic — essential on whitelist networks
     * where our UDP:53 probes get dropped but the carrier's own DoT/DoH
     * channel (reached through `Network.getAllByName`) still works.
     */
    @Volatile
    var underlyingNetwork: Network? = null

    /**
     * Called FROM Rust (via JNI callback) to protect a socket fd.
     * Protected sockets bypass the VPN tunnel — critical to avoid routing loops.
     */
    @JvmStatic
    fun protectSocket(fd: Int): Boolean {
        return current?.protect(fd) ?: false
    }

    /**
     * Called FROM Rust to resolve a hostname via the underlying non-VPN
     * network. Returns an IPv4 literal or null on any failure (no underlying
     * network, unknown host, only-IPv6 result, …). Rust treats null as
     * "fall through to UDP:53 fallback."
     *
     * Blocking — Rust invokes it from `tokio::task::spawn_blocking`.
     */
    @JvmStatic
    fun resolveDomain(host: String): String? {
        val network = underlyingNetwork ?: return null
        return try {
            // We need an IPv4 for the downstream protected-TCP connect path
            // (see session.rs relay_direct IPv4 match). Iterate and pick
            // the first IPv4 answer — Android may return IPv6 first when
            // the carrier has both.
            val answers = network.getAllByName(host) ?: return null
            answers.firstOrNull { it.address.size == 4 }?.hostAddress
        } catch (_: Exception) {
            // UnknownHost, SecurityException, network-unreachable — treat
            // all as "resolver couldn't help", let Rust fall back.
            null
        }
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

    // ── Onboarding (LLD-04) ─────────────────────────────────────────
    // All functions return JSON strings — parse on Kotlin side.

    /** Parse a raw URL (scanned / pasted / deep-linked). Returns either
     *  `{"kind":"https|custom","hub_url":..,"token":..}` or `{"error":".."}`. */
    external fun nativeParseInviteLink(raw: String): String

    /** GET invite metadata (does NOT consume). Returns InviteInfo JSON
     *  (fields: token, preset, comment, status, expires_at) or `{"error":".."}`. */
    external fun nativeFetchInviteInfo(hubUrl: String, token: String, timeoutMs: Long): String

    /** Claim + TOFU public key + pre-warm preset cache. Returns JSON:
     *  `{"payload":..?,"public_key":..?,"preset_cached":bool,"errors":[..]}`.
     *  `payload` null means the whole apply failed — check `errors`. */
    external fun nativeApplyInvite(
        hubUrl: String,
        token: String,
        preset: String,
        cacheDir: String,
        timeoutMs: Long,
    ): String
}
