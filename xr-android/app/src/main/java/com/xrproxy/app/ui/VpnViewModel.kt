package com.xrproxy.app.ui

import android.app.Application
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.net.VpnService
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.service.XrVpnService
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch

data class VpnUiState(
    val connected: Boolean = false,
    val connecting: Boolean = false,
    val state: String = "Disconnected",
    val bytesUp: Long = 0,
    val bytesDown: Long = 0,
    val activeConnections: Int = 0,
    val uptime: Long = 0,
    // Settings
    val serverAddress: String = "",
    val serverPort: String = "8443",
    val obfuscationKey: String = "",
    val modifier: String = "positional_xor_rotate",
    val salt: String = "3735928559",
    // Routing
    val routingPreset: String = "russia", // "russia", "proxy_all", "custom"
    val customDomains: String = "",
    val customIpRanges: String = "",
    // UI feedback
    val settingsSaved: Boolean = false,
)

class VpnViewModel(application: Application) : AndroidViewModel(application) {

    private val prefs: SharedPreferences =
        application.getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)

    private val _uiState = MutableStateFlow(VpnUiState())
    val uiState: StateFlow<VpnUiState> = _uiState

    private var statsPolling = false

    init {
        loadSettings()
    }

    // ── Settings persistence ────────────────────────────────────────

    private fun loadSettings() {
        _uiState.value = _uiState.value.copy(
            serverAddress = prefs.getString("server_address", "") ?: "",
            serverPort = prefs.getString("server_port", "8443") ?: "8443",
            obfuscationKey = prefs.getString("obfuscation_key", "") ?: "",
            modifier = prefs.getString("modifier", "positional_xor_rotate") ?: "positional_xor_rotate",
            salt = prefs.getString("salt", "3735928559") ?: "3735928559",
            routingPreset = prefs.getString("routing_preset", "russia") ?: "russia",
            customDomains = prefs.getString("custom_domains", "") ?: "",
            customIpRanges = prefs.getString("custom_ip_ranges", "") ?: "",
        )
    }

    fun saveSettings() {
        val s = _uiState.value
        prefs.edit()
            .putString("server_address", s.serverAddress)
            .putString("server_port", s.serverPort)
            .putString("obfuscation_key", s.obfuscationKey)
            .putString("modifier", s.modifier)
            .putString("salt", s.salt)
            .putString("routing_preset", s.routingPreset)
            .putString("custom_domains", s.customDomains)
            .putString("custom_ip_ranges", s.customIpRanges)
            .apply()

        _uiState.value = _uiState.value.copy(settingsSaved = true)
        viewModelScope.launch {
            delay(2000)
            _uiState.value = _uiState.value.copy(settingsSaved = false)
        }
    }

    // ── Field updates ───────────────────────────────────────────────

    fun updateServerAddress(value: String) { _uiState.value = _uiState.value.copy(serverAddress = value) }
    fun updateServerPort(value: String) { _uiState.value = _uiState.value.copy(serverPort = value) }
    fun updateObfuscationKey(value: String) { _uiState.value = _uiState.value.copy(obfuscationKey = value) }
    fun updateSalt(value: String) { _uiState.value = _uiState.value.copy(salt = value) }
    fun updateRoutingPreset(value: String) { _uiState.value = _uiState.value.copy(routingPreset = value) }
    fun updateCustomDomains(value: String) { _uiState.value = _uiState.value.copy(customDomains = value) }
    fun updateCustomIpRanges(value: String) { _uiState.value = _uiState.value.copy(customIpRanges = value) }

    /// Import TOML config from clipboard text.
    fun importToml(toml: String) {
        // Parse domains and ip_ranges from TOML.
        val domains = mutableListOf<String>()
        val ipRanges = mutableListOf<String>()
        var defaultAction = "direct"

        for (line in toml.lines()) {
            val trimmed = line.trim()
            if (trimmed.startsWith("default_action")) {
                if (trimmed.contains("proxy")) defaultAction = "proxy"
                else defaultAction = "direct"
            }
            // Extract quoted strings from domains = [...] and ip_ranges = [...]
            val quoted = Regex("\"([^\"]+)\"").findAll(trimmed)
            if (trimmed.startsWith("domains") || trimmed.contains("\"*.") || trimmed.contains("\".")) {
                // Heuristic: lines with domain patterns.
            }
        }

        // Simpler approach: extract all quoted strings from domain/ip sections.
        var inDomains = false
        var inIpRanges = false
        for (line in toml.lines()) {
            val t = line.trim()
            if (t.startsWith("domains")) inDomains = true
            if (t.startsWith("ip_ranges")) { inDomains = false; inIpRanges = true }
            if (t.startsWith("action") || t.startsWith("[[") || t.startsWith("[routing")) {
                inDomains = false; inIpRanges = false
            }

            val matches = Regex("\"([^\"]+)\"").findAll(t)
            for (m in matches) {
                val v = m.groupValues[1]
                if (inDomains && (v.contains(".") || v.startsWith("*"))) domains.add(v)
                if (inIpRanges && v.contains("/")) ipRanges.add(v)
            }
        }

        if (domains.isNotEmpty() || ipRanges.isNotEmpty()) {
            _uiState.value = _uiState.value.copy(
                routingPreset = "custom",
                customDomains = domains.joinToString("\n"),
                customIpRanges = ipRanges.joinToString("\n"),
            )
        }
    }

    // ── VPN connection ──────────────────────────────────────────────

    fun prepareVpn(): Intent? = VpnService.prepare(getApplication())

    fun connect() {
        val state = _uiState.value
        if (state.serverAddress.isBlank() || state.obfuscationKey.isBlank()) return

        // Save settings before connecting.
        saveSettings()

        _uiState.value = state.copy(connecting = true, state = "Connecting...")

        val configJson = buildConfigJson(_uiState.value)
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_START
            putExtra(XrVpnService.EXTRA_CONFIG_JSON, configJson)
        }
        getApplication<Application>().startForegroundService(intent)
        startStatsPolling()
    }

    fun disconnect() {
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_STOP
        }
        getApplication<Application>().startService(intent)

        statsPolling = false
        _uiState.value = _uiState.value.copy(
            connected = false, connecting = false, state = "Disconnected",
            bytesUp = 0, bytesDown = 0, activeConnections = 0, uptime = 0,
        )
    }

    private fun startStatsPolling() {
        if (statsPolling) return
        statsPolling = true

        viewModelScope.launch {
            while (statsPolling) {
                val stateStr = NativeBridge.nativeGetState()
                val connected = stateStr == "Connected"
                val connecting = stateStr == "Connecting"

                val statsJson = NativeBridge.nativeGetStats()
                val bytesUp = extractLong(statsJson, "bytes_up")
                val bytesDown = extractLong(statsJson, "bytes_down")
                val active = extractLong(statsJson, "active").toInt()
                val uptime = extractLong(statsJson, "uptime")

                _uiState.value = _uiState.value.copy(
                    connected = connected, connecting = connecting, state = stateStr,
                    bytesUp = bytesUp, bytesDown = bytesDown,
                    activeConnections = active, uptime = uptime,
                )

                if (!connected && !connecting) { statsPolling = false; break }
                delay(1000)
            }
        }
    }

    // ── Config building ─────────────────────────────────────────────

    private fun buildConfigJson(state: VpnUiState): String {
        val routingToml = buildRoutingToml(state)
        return """
            {
                "server_address": "${state.serverAddress}",
                "server_port": ${state.serverPort},
                "obfuscation_key": "${state.obfuscationKey}",
                "modifier": "${state.modifier}",
                "salt": ${state.salt},
                "padding_min": 16,
                "padding_max": 128,
                "routing_toml": "${routingToml.replace("\"", "\\\"").replace("\n", "\\n")}",
                "on_server_down": "direct"
            }
        """.trimIndent()
    }

    private fun buildRoutingToml(state: VpnUiState): String {
        return when (state.routingPreset) {
            "proxy_all" -> "default_action = \"proxy\"\n"
            "russia" -> PRESET_RUSSIA
            "custom" -> buildCustomRoutingToml(state)
            else -> PRESET_RUSSIA
        }
    }

    private fun buildCustomRoutingToml(state: VpnUiState): String {
        val sb = StringBuilder()
        sb.appendLine("default_action = \"direct\"")

        val domains = state.customDomains.lines().map { it.trim() }.filter { it.isNotBlank() }
        val ipRanges = state.customIpRanges.lines().map { it.trim() }.filter { it.isNotBlank() }

        if (domains.isNotEmpty() || ipRanges.isNotEmpty()) {
            sb.appendLine("[[rules]]")
            sb.appendLine("action = \"proxy\"")
            if (domains.isNotEmpty()) {
                sb.append("domains = [")
                sb.append(domains.joinToString(", ") { "\"$it\"" })
                sb.appendLine("]")
            }
            if (ipRanges.isNotEmpty()) {
                sb.append("ip_ranges = [")
                sb.append(ipRanges.joinToString(", ") { "\"$it\"" })
                sb.appendLine("]")
            }
        }
        return sb.toString()
    }

    private fun extractLong(json: String, key: String): Long {
        val pattern = "\"$key\":"
        val idx = json.indexOf(pattern)
        if (idx < 0) return 0
        val rest = json.substring(idx + pattern.length).trimStart()
        return rest.takeWhile { it.isDigit() }.toLongOrNull() ?: 0
    }

    companion object {
        /** Preset routing rules for Russia — embedded from configs/routing-russia.toml */
        val PRESET_RUSSIA = """
default_action = "direct"
[[rules]]
action = "proxy"
domains = ["youtube.com", "*.youtube.com", "youtu.be", "*.googlevideo.com", "*.ytimg.com", "youtube-nocookie.com", "*.youtube-nocookie.com", "*.ggpht.com"]
[[rules]]
action = "proxy"
domains = ["facebook.com", "*.facebook.com", "fbcdn.net", "*.fbcdn.net", "instagram.com", "*.instagram.com", "*.cdninstagram.com", "threads.net", "*.threads.net", "whatsapp.com", "*.whatsapp.com", "whatsapp.net", "*.whatsapp.net", "wa.me", "messenger.com", "*.messenger.com", "meta.com", "*.meta.com"]
[[rules]]
action = "proxy"
domains = ["twitter.com", "*.twitter.com", "x.com", "*.x.com", "t.co", "twimg.com", "*.twimg.com"]
[[rules]]
action = "proxy"
domains = ["telegram.org", "*.telegram.org", "t.me", "*.t.me", "telegram.me", "*.telegram.me", "telesco.pe"]
ip_ranges = ["91.108.56.0/22", "91.108.4.0/22", "91.108.8.0/22", "91.108.16.0/22", "91.108.12.0/22", "91.108.20.0/22", "149.154.160.0/20", "91.105.192.0/23", "185.76.151.0/24"]
[[rules]]
action = "proxy"
domains = ["linkedin.com", "*.linkedin.com", "licdn.com", "*.licdn.com"]
[[rules]]
action = "proxy"
domains = ["snapchat.com", "*.snapchat.com", "snap.com", "*.snap.com", "*.sc-cdn.net"]
[[rules]]
action = "proxy"
domains = ["google.com", "*.google.com", "*.googleapis.com", "*.gstatic.com", "*.googleusercontent.com", "gmail.com", "*.gmail.com", "*.gvt1.com", "*.gvt2.com"]
[[rules]]
action = "proxy"
domains = ["discord.com", "*.discord.com", "discord.gg", "discord.media", "*.discord.media", "discordapp.com", "*.discordapp.com", "discordapp.net", "*.discordapp.net"]
[[rules]]
action = "proxy"
domains = ["twitch.tv", "*.twitch.tv", "spotify.com", "*.spotify.com", "scdn.co", "*.scdn.co", "soundcloud.com", "*.soundcloud.com", "medium.com", "*.medium.com", "patreon.com", "*.patreon.com"]
[[rules]]
action = "proxy"
domains = ["openai.com", "*.openai.com", "chatgpt.com", "*.chatgpt.com", "claude.ai", "*.claude.ai", "anthropic.com", "*.anthropic.com"]
[[rules]]
action = "proxy"
domains = ["github.com", "*.github.com", "github.io", "*.github.io", "githubusercontent.com", "*.githubusercontent.com", "docker.io", "*.docker.io", "docker.com", "*.docker.com", "npmjs.com", "*.npmjs.com", "stackoverflow.com", "*.stackoverflow.com"]
[[rules]]
action = "proxy"
domains = ["protonvpn.com", "*.protonvpn.com", "proton.me", "*.proton.me", "signal.org", "*.signal.org"]
[[rules]]
action = "proxy"
domains = ["bbc.com", "*.bbc.com", "bbc.co.uk", "*.bbc.co.uk", "*.bbci.co.uk", "dw.com", "*.dw.com", "svoboda.org", "*.svoboda.org"]
[[rules]]
action = "proxy"
domains = ["notion.so", "*.notion.so", "notion.com", "*.notion.com", "figma.com", "*.figma.com", "cloudflare.com", "*.cloudflare.com"]
        """.trimIndent()
    }
}
