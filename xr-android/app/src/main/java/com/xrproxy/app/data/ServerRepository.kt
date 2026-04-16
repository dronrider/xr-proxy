package com.xrproxy.app.data

import android.content.SharedPreferences
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONArray
import org.json.JSONObject
import java.time.OffsetDateTime
import java.util.UUID

class ServerRepository(private val prefs: SharedPreferences) {

    private val _servers = MutableStateFlow<List<ServerProfile>>(emptyList())
    val servers: StateFlow<List<ServerProfile>> = _servers

    private val _activeId = MutableStateFlow<String?>(null)
    val activeId: StateFlow<String?> = _activeId

    init {
        load()
        migrateFromFlatPrefsIfNeeded()
    }

    fun activeServer(): ServerProfile? =
        _servers.value.firstOrNull { it.id == _activeId.value }

    fun upsert(profile: ServerProfile) {
        val list = _servers.value.toMutableList()
        val idx = list.indexOfFirst { it.id == profile.id }
        if (idx >= 0) list[idx] = profile else list.add(profile)
        _servers.value = list
        save()
    }

    fun delete(id: String) {
        _servers.value = _servers.value.filter { it.id != id }
        if (_activeId.value == id) {
            _activeId.value = _servers.value.firstOrNull()?.id
        }
        save()
    }

    fun setActive(id: String) {
        if (_servers.value.any { it.id == id }) {
            _activeId.value = id
            save()
        }
    }

    fun hasDuplicate(address: String, port: Int, excludeId: String? = null): ServerProfile? =
        _servers.value.firstOrNull {
            it.serverAddress == address && it.serverPort == port && it.id != excludeId
        }

    fun generateName(address: String, hubUrl: String, comment: String): String = when {
        comment.isNotBlank() -> comment
        hubUrl.isNotBlank() -> hostOf(hubUrl)
        address.isNotBlank() -> address
        else -> "Server ${_servers.value.size + 1}"
    }

    // ── Persistence ─────────────────────────────────────────────────

    private fun load() {
        val raw = prefs.getString(KEY_SERVERS, null)
        if (raw == null) {
            _servers.value = emptyList()
            _activeId.value = null
            return
        }
        try {
            val arr = JSONArray(raw)
            val list = (0 until arr.length()).map { parseProfile(arr.getJSONObject(it)) }
            _servers.value = list
            _activeId.value = prefs.getString(KEY_ACTIVE_ID, null)
                ?: list.firstOrNull()?.id
        } catch (e: Exception) {
            Log.w("ServerRepo", "corrupted servers JSON, resetting: $e")
            _servers.value = emptyList()
            _activeId.value = null
        }
    }

    private fun save() {
        val arr = JSONArray()
        for (p in _servers.value) arr.put(toJson(p))
        prefs.edit()
            .putString(KEY_SERVERS, arr.toString())
            .putString(KEY_ACTIVE_ID, _activeId.value)
            .apply()
    }

    // ── Migration from flat prefs (LLD-08 §3.10) ───────────────────

    private fun migrateFromFlatPrefsIfNeeded() {
        if (prefs.contains(KEY_SERVERS)) return
        val addr = prefs.getString("server_address", "") ?: ""
        if (addr.isBlank()) {
            save()
            return
        }
        val hubUrl = prefs.getString("hub_url", "") ?: ""
        val profile = ServerProfile(
            id = UUID.randomUUID().toString(),
            name = if (hubUrl.isNotBlank()) hostOf(hubUrl) else addr,
            serverAddress = addr,
            serverPort = prefs.getString("server_port", "8443")?.toIntOrNull() ?: 8443,
            obfuscationKey = prefs.getString("obfuscation_key", "") ?: "",
            modifier = prefs.getString("modifier", "positional_xor_rotate")
                ?: "positional_xor_rotate",
            salt = prefs.getString("salt", "3735928559")?.toLongOrNull() ?: 0xDEADBEEFL,
            routingPreset = prefs.getString("routing_preset", "russia") ?: "russia",
            customDomains = prefs.getString("custom_domains", "") ?: "",
            customIpRanges = prefs.getString("custom_ip_ranges", "") ?: "",
            hubUrl = hubUrl,
            hubPreset = prefs.getString("hub_preset", "") ?: "",
            trustedPublicKey = prefs.getString("trusted_public_key", "") ?: "",
            createdAt = OffsetDateTime.now().toString(),
            source = if (hubUrl.isNotBlank()) ServerSource.Invite else ServerSource.Manual,
        )
        _servers.value = listOf(profile)
        _activeId.value = profile.id
        save()
    }

    // ── JSON serialization ──────────────────────────────────────────

    private fun toJson(p: ServerProfile): JSONObject = JSONObject().apply {
        put("id", p.id)
        put("name", p.name)
        put("server_address", p.serverAddress)
        put("server_port", p.serverPort)
        put("obfuscation_key", p.obfuscationKey)
        put("modifier", p.modifier)
        put("salt", p.salt)
        put("routing_preset", p.routingPreset)
        put("custom_domains", p.customDomains)
        put("custom_ip_ranges", p.customIpRanges)
        put("hub_url", p.hubUrl)
        put("hub_preset", p.hubPreset)
        put("trusted_public_key", p.trustedPublicKey)
        put("created_at", p.createdAt)
        put("source", p.source.name)
    }

    private fun parseProfile(j: JSONObject): ServerProfile = ServerProfile(
        id = j.optString("id", UUID.randomUUID().toString()),
        name = j.optString("name", ""),
        serverAddress = j.optString("server_address", ""),
        serverPort = j.optInt("server_port", 8443),
        obfuscationKey = j.optString("obfuscation_key", ""),
        modifier = j.optString("modifier", "positional_xor_rotate"),
        salt = j.optLong("salt", 0xDEADBEEFL),
        routingPreset = j.optString("routing_preset", "russia"),
        customDomains = j.optString("custom_domains", ""),
        customIpRanges = j.optString("custom_ip_ranges", ""),
        hubUrl = j.optString("hub_url", ""),
        hubPreset = j.optString("hub_preset", ""),
        trustedPublicKey = j.optString("trusted_public_key", ""),
        createdAt = j.optString("created_at", OffsetDateTime.now().toString()),
        source = try {
            ServerSource.valueOf(j.optString("source", "Manual"))
        } catch (_: Exception) {
            ServerSource.Manual
        },
    )

    companion object {
        private const val KEY_SERVERS = "servers"
        private const val KEY_ACTIVE_ID = "active_server_id"

        fun hostOf(url: String): String = try {
            java.net.URI(url).host ?: url
        } catch (_: Exception) {
            url
        }
    }
}
