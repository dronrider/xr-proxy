package com.xrproxy.app.data

import android.content.SharedPreferences
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONArray

/**
 * Persisted list of "trusted" Wi-Fi networks (by SSID) for the auto-pause
 * feature (task 3b-2). When the phone joins one of these networks — typically
 * the home network already behind an xr-client router — the app pauses its own
 * tunnel to avoid double-tunnelling. See `XrVpnService` for the watcher that
 * consumes this, and `xr_core::trusted` for the matching logic.
 *
 * Backed by the same `"xr_proxy"` SharedPreferences as [ServerRepository], so
 * the VPN service and the UI ViewModel each hold their own instance over the
 * one process-wide backing map. The UI mutates through the StateFlow-backed
 * helpers; the service reads the authoritative state fresh from prefs via
 * [activeTrustedSsids]/[isEnabled] on every network change (writes via
 * `apply()` are visible across instances immediately).
 */
class TrustedNetworksRepository(private val prefs: SharedPreferences) {

    private val _networks = MutableStateFlow<List<String>>(emptyList())
    val networks: StateFlow<List<String>> = _networks

    private val _enabled = MutableStateFlow(true)
    val enabled: StateFlow<Boolean> = _enabled

    init {
        reload()
    }

    /** Re-read state from prefs into the StateFlows (UI side). */
    fun reload() {
        _networks.value = readNetworks()
        _enabled.value = prefs.getBoolean(KEY_ENABLED, true)
    }

    /**
     * Add an SSID to the trusted list. Trims whitespace and de-duplicates
     * case-insensitively. Blank names are ignored.
     */
    fun add(ssid: String) {
        val clean = ssid.trim()
        if (clean.isEmpty()) return
        val current = readNetworks()
        if (current.any { it.equals(clean, ignoreCase = true) }) return
        save(current + clean)
    }

    fun remove(ssid: String) {
        save(readNetworks().filterNot { it.equals(ssid, ignoreCase = true) })
    }

    fun setEnabled(value: Boolean) {
        prefs.edit().putBoolean(KEY_ENABLED, value).apply()
        _enabled.value = value
    }

    /** Authoritative read for the service: true if auto-pause is enabled. */
    fun isEnabled(): Boolean = prefs.getBoolean(KEY_ENABLED, true)

    /**
     * Authoritative trusted-SSID list for matching, read fresh from prefs.
     * Returns an empty array when the feature is disabled — so a disabled
     * toggle alone short-circuits the watcher without it inspecting the list.
     */
    fun activeTrustedSsids(): Array<String> =
        if (isEnabled()) readNetworks().toTypedArray() else emptyArray()

    // ── Persistence ─────────────────────────────────────────────────

    private fun readNetworks(): List<String> {
        val raw = prefs.getString(KEY_NETWORKS, null) ?: return emptyList()
        return try {
            val arr = JSONArray(raw)
            (0 until arr.length()).mapNotNull { arr.optString(it, "").takeIf { s -> s.isNotBlank() } }
        } catch (e: Exception) {
            Log.w("TrustedNetworks", "corrupted trusted-networks JSON, resetting: $e")
            emptyList()
        }
    }

    private fun save(list: List<String>) {
        val arr = JSONArray()
        for (s in list) arr.put(s)
        prefs.edit().putString(KEY_NETWORKS, arr.toString()).apply()
        _networks.value = list
    }

    companion object {
        private const val KEY_NETWORKS = "trusted_networks"
        private const val KEY_ENABLED = "trusted_networks_enabled"
    }
}
