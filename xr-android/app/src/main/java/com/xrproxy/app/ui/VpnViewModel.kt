package com.xrproxy.app.ui

import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.SharedPreferences
import android.net.VpnService
import android.os.IBinder
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.service.XrVpnService
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch

/** High-level phase of the VPN session as seen by the UI. */
enum class ConnectPhase {
    Idle,
    NeedsPermission,
    Starting,
    Connecting,
    Connected,
    Stopping,
}

data class VpnUiState(
    val phase: ConnectPhase = ConnectPhase.Idle,
    val state: String = "Disconnected",
    val bytesUp: Long = 0,
    val bytesDown: Long = 0,
    val activeConnections: Int = 0,
    val uptime: Long = 0,
    // Debug
    val dnsQueries: Long = 0,
    val tcpSyns: Long = 0,
    val smolRecv: Long = 0,
    val smolSend: Long = 0,
    val relayWarnings: Long = 0,
    val relayErrors: Long = 0,
    val debugMsg: String = "",
    val recentErrors: List<String> = emptyList(),
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
) {
    val connected: Boolean
        get() = phase == ConnectPhase.Connected
    val connecting: Boolean
        get() = phase == ConnectPhase.Starting ||
                phase == ConnectPhase.Connecting ||
                phase == ConnectPhase.Stopping
}

class VpnViewModel(application: Application) : AndroidViewModel(application) {

    private val prefs: SharedPreferences =
        application.getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)

    private val _uiState = MutableStateFlow(VpnUiState())
    val uiState: StateFlow<VpnUiState> = _uiState

    private val _permissionRequest = MutableSharedFlow<Intent>(extraBufferCapacity = 1)
    val permissionRequest: SharedFlow<Intent> = _permissionRequest

    private val _messages = MutableSharedFlow<String>(extraBufferCapacity = 4)
    val messages: SharedFlow<String> = _messages

    private var boundService: XrVpnService? = null
    private var isBound = false
    private var serviceObserverJob: Job? = null

    private val bindConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val svc = (binder as XrVpnService.LocalBinder).service()
            boundService = svc
            serviceObserverJob?.cancel()
            serviceObserverJob = viewModelScope.launch {
                svc.stateFlow.collect { applyServiceState(it) }
            }
        }

        override fun onServiceDisconnected(name: ComponentName) {
            boundService = null
            serviceObserverJob?.cancel()
            serviceObserverJob = null
            isBound = false
            // Service went away unexpectedly — reflect Disconnected honestly.
            _uiState.value = _uiState.value.copy(
                phase = ConnectPhase.Idle,
                state = "Disconnected",
                bytesUp = 0, bytesDown = 0, activeConnections = 0, uptime = 0,
                recentErrors = emptyList(),
            )
        }
    }

    /**
     * Releases the current binding. Called after the service reports Idle
     * (in `applyServiceState`) so the service can actually be destroyed —
     * `stopSelf()` alone is a no-op while we keep a binding alive.
     */
    private fun unbindAndClear() {
        serviceObserverJob?.cancel()
        serviceObserverJob = null
        boundService = null
        if (isBound) {
            try {
                getApplication<Application>().unbindService(bindConnection)
            } catch (_: Exception) {
                // Already disconnected — ignore.
            }
            isBound = false
        }
    }

    init {
        loadSettings()
        // Best-effort bind: if the service is already running (app re-entered
        // from background), we immediately mirror its state. If it isn't, we
        // stay Idle and bind again when the user hits Connect.
        tryBind(autoCreate = false)
    }

    override fun onCleared() {
        serviceObserverJob?.cancel()
        if (isBound) {
            try {
                getApplication<Application>().unbindService(bindConnection)
            } catch (_: Exception) {
                // Already disconnected or never bound — ignore.
            }
            isBound = false
        }
        super.onCleared()
    }

    private fun tryBind(autoCreate: Boolean) {
        if (isBound) return
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_BIND_INTERNAL
        }
        val flags = if (autoCreate) Context.BIND_AUTO_CREATE else 0
        isBound = try {
            getApplication<Application>().bindService(intent, bindConnection, flags)
        } catch (_: Exception) {
            false
        }
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

    fun clearLog() {
        boundService?.clearLog()
    }

    /// Import TOML config from clipboard text.
    fun importToml(toml: String) {
        // Parse domains and ip_ranges from TOML.
        val domains = mutableListOf<String>()
        val ipRanges = mutableListOf<String>()

        // Extract all quoted strings from domain/ip sections.
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

    /**
     * Single entry point for the Connect button. Validates settings, requests
     * VPN permission if needed, then starts the service. The actual server
     * reachability check happens inside the Rust engine via a protected socket
     * health check — the same code path used by real relay connections.
     * A plain-socket probe from Kotlin is unreliable on Android: carrier NATs
     * and port filtering cause false negatives on non-standard ports.
     */
    fun onConnectClicked() {
        val s = _uiState.value
        if (s.phase != ConnectPhase.Idle) return
        if (s.serverAddress.isBlank() || s.obfuscationKey.isBlank()) {
            viewModelScope.launch { _messages.emit("Заполните сервер и ключ в Settings") }
            return
        }

        _uiState.value = s.copy(phase = ConnectPhase.Starting, state = "Connecting...")

        val intent: Intent? = try {
            VpnService.prepare(getApplication())
        } catch (_: Exception) {
            null
        }
        if (intent == null) {
            actuallyStart()
        } else {
            _uiState.value = _uiState.value.copy(phase = ConnectPhase.NeedsPermission)
            viewModelScope.launch { _permissionRequest.emit(intent) }
        }
    }

    /** Called from Activity launcher callback, regardless of result code. */
    fun onPermissionResult(granted: Boolean) {
        if (granted) {
            actuallyStart()
        } else {
            _uiState.value = _uiState.value.copy(
                phase = ConnectPhase.Idle,
                state = "Disconnected",
            )
            viewModelScope.launch { _messages.emit("VPN-разрешение не получено") }
        }
    }

    private fun actuallyStart() {
        saveSettings()
        val configJson = buildConfigJson(_uiState.value)
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_START
            putExtra(XrVpnService.EXTRA_CONFIG_JSON, configJson)
        }
        getApplication<Application>().startForegroundService(intent)
        // After startForegroundService the service is created; bind with
        // BIND_AUTO_CREATE to ride out the race where the service isn't
        // yet fully up when we call bindService.
        tryBind(autoCreate = true)
        _uiState.value = _uiState.value.copy(phase = ConnectPhase.Starting, state = "Connecting...")
    }

    fun disconnect() {
        val svc = boundService
        if (svc != null) {
            // stopFromUi запускает корутину в сервисе, которая дойдёт до
            // publish(Phase.Idle). applyServiceState увидит Idle и вызовет
            // unbindAndClear — сервис тогда реально умрёт, и следующий
            // Connect получит свежий instance. UI обновится через stateFlow.
            svc.stopFromUi()
            return
        }
        // Binder'а нет (init-bind не нашёл живого сервиса, actuallyStart
        // ещё не успел привязаться). Fallback через intent ACTION_STOP
        // + локально выставить Idle, потому что stateFlow нам ничего
        // не пришлёт.
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_STOP
        }
        try {
            getApplication<Application>().startService(intent)
        } catch (_: Exception) {
            // Ignore — nothing to stop.
        }
        _uiState.value = _uiState.value.copy(
            phase = ConnectPhase.Idle,
            state = "Disconnected",
            bytesUp = 0, bytesDown = 0, activeConnections = 0, uptime = 0,
            recentErrors = emptyList(),
        )
    }

    private fun applyServiceState(svcState: XrVpnService.ServiceState) {
        val phase = when (svcState.phase) {
            XrVpnService.Phase.Idle -> ConnectPhase.Idle
            XrVpnService.Phase.Preparing -> ConnectPhase.Starting
            XrVpnService.Phase.Connecting -> ConnectPhase.Connecting
            XrVpnService.Phase.Connected -> ConnectPhase.Connected
            XrVpnService.Phase.Stopping -> ConnectPhase.Stopping
            XrVpnService.Phase.Error -> ConnectPhase.Idle
        }
        val stateStr = when (svcState.phase) {
            XrVpnService.Phase.Idle -> "Disconnected"
            XrVpnService.Phase.Preparing -> "Preparing..."
            XrVpnService.Phase.Connecting -> "Connecting..."
            XrVpnService.Phase.Connected -> "Connected"
            XrVpnService.Phase.Stopping -> "Disconnecting..."
            XrVpnService.Phase.Error -> "Error"
        }
        val snap = svcState.snapshot
        _uiState.value = _uiState.value.copy(
            phase = phase,
            state = stateStr,
            bytesUp = snap?.bytesUp ?: 0,
            bytesDown = snap?.bytesDown ?: 0,
            activeConnections = snap?.activeConnections ?: 0,
            uptime = snap?.uptime ?: 0,
            dnsQueries = snap?.dnsQueries ?: 0,
            tcpSyns = snap?.tcpSyns ?: 0,
            smolRecv = snap?.smolRecv ?: 0,
            smolSend = snap?.smolSend ?: 0,
            relayWarnings = snap?.relayWarnings ?: 0,
            relayErrors = snap?.relayErrors ?: 0,
            debugMsg = snap?.debugMsg ?: "",
            recentErrors = snap?.recentErrors ?: emptyList(),
        )
        if (svcState.phase == XrVpnService.Phase.Error && svcState.errorMessage != null) {
            viewModelScope.launch { _messages.emit(svcState.errorMessage) }
        }
        if ((svcState.phase == XrVpnService.Phase.Idle ||
             svcState.phase == XrVpnService.Phase.Error) && isBound) {
            // Сервис завершил свою работу (нормально или по ошибке) и ждёт
            // unbind, чтобы реально умереть. Отпускаем binding — `onDestroy`
            // сервиса вычистит `NativeBridge.current` и `scope`, следующий
            // Connect получит свежий instance через `startForegroundService`.
            viewModelScope.launch { unbindAndClear() }
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
