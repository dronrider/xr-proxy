package com.xrproxy.app.ui

import android.app.Application
import android.content.Intent
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
    val defaultAction: String = "proxy",
    // Custom rules
    val customDomains: String = "",
    val customIpRanges: String = "",
)

class VpnViewModel(application: Application) : AndroidViewModel(application) {

    private val _uiState = MutableStateFlow(VpnUiState())
    val uiState: StateFlow<VpnUiState> = _uiState

    private var statsPolling = false

    fun updateServerAddress(value: String) {
        _uiState.value = _uiState.value.copy(serverAddress = value)
    }

    fun updateServerPort(value: String) {
        _uiState.value = _uiState.value.copy(serverPort = value)
    }

    fun updateObfuscationKey(value: String) {
        _uiState.value = _uiState.value.copy(obfuscationKey = value)
    }

    fun updateModifier(value: String) {
        _uiState.value = _uiState.value.copy(modifier = value)
    }

    fun updateSalt(value: String) {
        _uiState.value = _uiState.value.copy(salt = value)
    }

    fun updateDefaultAction(value: String) {
        _uiState.value = _uiState.value.copy(defaultAction = value)
    }

    fun updateCustomDomains(value: String) {
        _uiState.value = _uiState.value.copy(customDomains = value)
    }

    fun updateCustomIpRanges(value: String) {
        _uiState.value = _uiState.value.copy(customIpRanges = value)
    }

    /** Check if VPN permission is granted. Returns intent to request if not. */
    fun prepareVpn(): Intent? {
        return VpnService.prepare(getApplication())
    }

    fun connect() {
        val state = _uiState.value
        if (state.serverAddress.isBlank() || state.obfuscationKey.isBlank()) return

        _uiState.value = state.copy(connecting = true)

        val configJson = buildConfigJson(state)
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
            connected = false,
            connecting = false,
            state = "Disconnected",
            bytesUp = 0,
            bytesDown = 0,
            activeConnections = 0,
            uptime = 0,
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

                // Simple JSON parsing for stats.
                val statsJson = NativeBridge.nativeGetStats()
                val bytesUp = extractLong(statsJson, "bytes_up")
                val bytesDown = extractLong(statsJson, "bytes_down")
                val active = extractLong(statsJson, "active").toInt()
                val uptime = extractLong(statsJson, "uptime")

                _uiState.value = _uiState.value.copy(
                    connected = connected,
                    connecting = connecting,
                    state = stateStr,
                    bytesUp = bytesUp,
                    bytesDown = bytesDown,
                    activeConnections = active,
                    uptime = uptime,
                )

                if (!connected && !connecting) {
                    statsPolling = false
                    break
                }

                delay(1000)
            }
        }
    }

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
        val sb = StringBuilder()
        sb.appendLine("default_action = \"${state.defaultAction}\"")

        val domains = state.customDomains
            .lines()
            .map { it.trim() }
            .filter { it.isNotBlank() }

        val ipRanges = state.customIpRanges
            .lines()
            .map { it.trim() }
            .filter { it.isNotBlank() }

        if (domains.isNotEmpty() || ipRanges.isNotEmpty()) {
            sb.appendLine("[[rules]]")
            // If default is "direct", custom rules proxy. And vice versa.
            val ruleAction = if (state.defaultAction == "direct") "proxy" else "direct"
            sb.appendLine("action = \"$ruleAction\"")

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
}
