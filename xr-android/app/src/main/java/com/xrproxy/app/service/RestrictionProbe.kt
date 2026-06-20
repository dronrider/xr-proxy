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
    // Generous timeout: the probe runs right after the tunnel pauses, when the
    // phone has just dropped its own VPN and the router's transparent-proxy mux
    // to the VPS is cold. The first proxied connection is slow; a tight 4s
    // budget read that as "blocked" on a network that actually proxies fine.
    private const val TIMEOUT_MS = 7000
    // Retry once: a single timeout on the first (cold) connection is not enough
    // to call a reliably-blocked host unreachable.
    private const val ATTEMPTS = 2

    data class Result(val restricted: Boolean, val checked: Int, val failed: Int)

    /**
     * Probe [PROBE_COUNT] blocked hosts over [network] (the physical uplink;
     * the tunnel is paused). [seed] rotates which hosts are picked. Blocking,
     * call off the main thread.
     *
     * The network is flagged restricted only if **every** probed host is
     * unreachable (after retries). On a network that proxies for us at least one
     * reliably-blocked host connects, so we short-circuit to "not restricted" on
     * the first success — which also keeps the healthy-network case fast. The
     * old 2-of-3 quorum cried wolf whenever the freshly-resumed direct path was
     * merely slow to warm up.
     */
    fun probe(network: Network?, seed: Int): Result {
        val hosts = pick(seed)
        var failed = 0
        for (host in hosts) {
            if (reachableWithRetry(network, host)) {
                return Result(restricted = false, checked = hosts.size, failed = failed)
            }
            failed++
        }
        return Result(restricted = hosts.isNotEmpty(), checked = hosts.size, failed = failed)
    }

    private fun reachableWithRetry(network: Network?, host: String): Boolean {
        repeat(ATTEMPTS) { if (reachable(network, host)) return true }
        return false
    }

    private fun pick(seed: Int): List<String> {
        if (CANDIDATES.size <= PROBE_COUNT) return CANDIDATES
        val start = ((seed % CANDIDATES.size) + CANDIDATES.size) % CANDIDATES.size
        return (0 until PROBE_COUNT).map { CANDIDATES[(start + it) % CANDIDATES.size] }
    }

    private fun reachable(network: Network?, host: String): Boolean {
        val addr: InetAddress = try {
            val answers = if (network != null) network.getAllByName(host)
            else InetAddress.getAllByName(host)
            // RKN DNS-MITM poisons blocked hosts to 127.0.0.1 / 0.0.0.0 — treat
            // a loopback/any-local answer as blocked.
            if (answers.any { it.isLoopbackAddress || it.isAnyLocalAddress }) return false
            // Force IPv4: the router's transparent proxy (TPROXY) is IPv4-only,
            // and home networks frequently advertise AAAA without working IPv6,
            // so an IPv6 attempt falsely times out (mirrors the engine's
            // resolver, which is IPv4-only downstream). A host with no A record
            // (IPv6-only, not poisoned) we can't fairly judge → not blocked.
            answers.firstOrNull { it is java.net.Inet4Address } ?: return true
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
