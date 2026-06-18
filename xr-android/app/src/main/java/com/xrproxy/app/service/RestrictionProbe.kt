package com.xrproxy.app.service

import android.net.Network
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import javax.net.ssl.SSLSocket
import javax.net.ssl.SSLSocketFactory

/**
 * Best-effort check of whether a trusted Wi-Fi actually provides unrestricted
 * access (task 3b-2, §2). The premise of pausing the tunnel on a "trusted"
 * network is that the network already proxies for us; if it doesn't (router
 * down, network mistakenly trusted, evil-twin SSID), pausing silently drops
 * access to blocked resources. So while paused — tunnel down, traffic direct —
 * we TLS-probe a few reliably RKN-blocked hosts over the physical uplink. A
 * quorum of failures means "this network has restrictions, warn the user".
 *
 * Heuristic, never throws: it reliably catches DPI/DNS blocking (the failure
 * mode the proxy exists to fix), but not app-level geo-blocks (those answer
 * 403 over a perfectly reachable TLS connection) — and shouldn't.
 */
object RestrictionProbe {

    // Concrete hostnames (no wildcards) that are network-blocked on a
    // restricted RU uplink — high signal. Ordered; we rotate the window so
    // repeated checks don't always hit the same hosts.
    private val CANDIDATES = listOf(
        "www.youtube.com",
        "www.instagram.com",
        "telegram.org",
        "x.com",
        "www.facebook.com",
    )
    private const val PROBE_COUNT = 3
    private const val TIMEOUT_MS = 3000
    private const val FAIL_THRESHOLD = 2 // >= this many unreachable → restricted

    data class Result(val restricted: Boolean, val checked: Int, val failed: Int)

    /**
     * Probe [PROBE_COUNT] blocked hosts over [network] (the physical uplink;
     * the tunnel is paused). [seed] rotates which hosts are picked. Blocking,
     * call off the main thread.
     */
    fun probe(network: Network?, seed: Int): Result {
        val hosts = pick(seed)
        val failed = hosts.count { !reachable(network, it) }
        return Result(restricted = failed >= FAIL_THRESHOLD, checked = hosts.size, failed = failed)
    }

    private fun pick(seed: Int): List<String> {
        if (CANDIDATES.size <= PROBE_COUNT) return CANDIDATES
        val start = ((seed % CANDIDATES.size) + CANDIDATES.size) % CANDIDATES.size
        return (0 until PROBE_COUNT).map { CANDIDATES[(start + it) % CANDIDATES.size] }
    }

    private fun reachable(network: Network?, host: String): Boolean {
        // 1) DNS. RKN DNS-MITM answers 127.0.0.1 for blocked hosts (see
        //    routers.md §4.2); a loopback/any-local answer == blocked.
        val addr: InetAddress = try {
            val answers = if (network != null) network.getAllByName(host)
            else InetAddress.getAllByName(host)
            answers.firstOrNull { !it.isLoopbackAddress && !it.isAnyLocalAddress }
                ?: return false
        } catch (_: Exception) {
            return false
        }

        // 2) TCP + TLS handshake with SNI = host. DPI SNI-blocking resets or
        //    times out here even when DNS is clean.
        var socket: Socket? = null
        return try {
            socket = network?.socketFactory?.createSocket() ?: Socket()
            socket.connect(InetSocketAddress(addr, 443), TIMEOUT_MS)
            socket.soTimeout = TIMEOUT_MS
            val ssl = (SSLSocketFactory.getDefault() as SSLSocketFactory)
                .createSocket(socket, host, 443, true) as SSLSocket
            ssl.startHandshake()
            ssl.close()
            true
        } catch (_: Exception) {
            false
        } finally {
            try { socket?.close() } catch (_: Exception) {}
        }
    }
}
