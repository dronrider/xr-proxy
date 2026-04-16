package com.xrproxy.app.ui

import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.VpnService
import android.os.IBinder
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.data.ServerRepository
import com.xrproxy.app.data.ServerSource
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.HealthLevel
import com.xrproxy.app.service.XrVpnService
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File
import java.time.OffsetDateTime
import java.util.UUID

enum class ConnectPhase {
    Idle,
    NeedsPermission,
    Preparing,
    Connecting,
    Finalizing,
    Connected,
    Stopping,
    ;

    val isTransitioning: Boolean
        get() = this == Preparing || this == Connecting || this == Finalizing || this == Stopping
}

enum class UiSeverity { Info, Warn, Error }
data class UiMessage(val text: String, val severity: UiSeverity = UiSeverity.Info)

sealed interface OnboardingState {
    object ShowingWelcome : OnboardingState
    object Loading : OnboardingState
    data class ConfirmInvite(
        val hubUrl: String,
        val token: String,
        val preset: String,
        val comment: String,
        val status: String,
        val expiresAt: String,
        val applyInProgress: Boolean = false,
    ) : OnboardingState
    object Completed : OnboardingState
}

data class VpnUiState(
    val phase: ConnectPhase = ConnectPhase.Idle,
    val state: String = "Disconnected",
    val bytesUp: Long = 0,
    val bytesDown: Long = 0,
    val activeConnections: Int = 0,
    val uptime: Long = 0,
    val speedUp: Long = 0,
    val speedDown: Long = 0,
    val health: HealthLevel = HealthLevel.Healthy,
    val dnsQueries: Long = 0,
    val tcpSyns: Long = 0,
    val smolRecv: Long = 0,
    val smolSend: Long = 0,
    val relayWarnings: Long = 0,
    val relayErrors: Long = 0,
    val debugMsg: String = "",
    val recentErrors: List<String> = emptyList(),
    val debugExpanded: Boolean = false,
) {
    val connected: Boolean
        get() = phase == ConnectPhase.Connected
    val connecting: Boolean
        get() = phase.isTransitioning
}

class VpnViewModel(application: Application) : AndroidViewModel(application) {

    private val prefs = application.getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)

    val repo = ServerRepository(prefs)

    private val _uiState = MutableStateFlow(VpnUiState())
    val uiState: StateFlow<VpnUiState> = _uiState

    private val _onboardingState = MutableStateFlow<OnboardingState>(OnboardingState.Loading)
    val onboardingState: StateFlow<OnboardingState> = _onboardingState

    private val _permissionRequest = MutableSharedFlow<Intent>(extraBufferCapacity = 1)
    val permissionRequest: SharedFlow<Intent> = _permissionRequest

    private val _messages = MutableSharedFlow<UiMessage>(extraBufferCapacity = 4)
    val messages: SharedFlow<UiMessage> = _messages

    private val presetCacheDir: File by lazy {
        File(getApplication<Application>().filesDir, "presets").also { it.mkdirs() }
    }

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
            _uiState.value = _uiState.value.copy(
                phase = ConnectPhase.Idle,
                state = "Disconnected",
                bytesUp = 0, bytesDown = 0, activeConnections = 0, uptime = 0,
                recentErrors = emptyList(),
            )
        }
    }

    private fun unbindAndClear() {
        serviceObserverJob?.cancel()
        serviceObserverJob = null
        boundService = null
        if (isBound) {
            try {
                getApplication<Application>().unbindService(bindConnection)
            } catch (_: Exception) {}
            isBound = false
        }
    }

    init {
        _onboardingState.value = initialOnboardingState()
        tryBind(autoCreate = false)
    }

    private fun initialOnboardingState(): OnboardingState =
        if (repo.servers.value.isEmpty()) OnboardingState.ShowingWelcome
        else OnboardingState.Completed

    override fun onCleared() {
        serviceObserverJob?.cancel()
        if (isBound) {
            try { getApplication<Application>().unbindService(bindConnection) } catch (_: Exception) {}
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
        } catch (_: Exception) { false }
    }

    // ── Server management (LLD-08) ──────────────────────────────────

    fun selectServer(id: String) {
        val s = _uiState.value
        if (s.phase != ConnectPhase.Idle && repo.activeId.value != id) {
            emitMessage("Сначала отключите VPN", UiSeverity.Warn)
            return
        }
        repo.setActive(id)
    }

    fun upsertServer(profile: ServerProfile) {
        repo.upsert(profile)
        if (repo.activeId.value == null) {
            repo.setActive(profile.id)
        }
    }

    fun deleteServer(id: String) {
        val isActive = repo.activeId.value == id
        if (isActive && _uiState.value.phase != ConnectPhase.Idle) {
            viewModelScope.launch {
                disconnect()
                _uiState.map { it.phase == ConnectPhase.Idle }.first { it }
                repo.delete(id)
                if (repo.servers.value.isEmpty()) {
                    _onboardingState.value = OnboardingState.ShowingWelcome
                }
            }
        } else {
            repo.delete(id)
            if (repo.servers.value.isEmpty()) {
                _onboardingState.value = OnboardingState.ShowingWelcome
            }
        }
    }

    fun onServerEditSaved(profile: ServerProfile) {
        val wasActive = repo.activeId.value == profile.id
        repo.upsert(profile)
        if (wasActive && _uiState.value.phase == ConnectPhase.Connected) {
            emitMessage("Применяю новые настройки…", UiSeverity.Info)
            viewModelScope.launch {
                disconnect()
                _uiState.map { it.phase == ConnectPhase.Idle }.first { it }
                delay(300)
                onConnectClicked()
            }
        }
    }

    fun clearLog() { boundService?.clearLog() }

    fun toggleDebug() {
        _uiState.value = _uiState.value.copy(debugExpanded = !_uiState.value.debugExpanded)
    }

    // ── Onboarding (LLD-04 + LLD-08) ───────────────────────────────

    fun onInviteLinkReceived(raw: String) {
        _onboardingState.value = OnboardingState.Loading
        viewModelScope.launch {
            val parsedJson = withContext(Dispatchers.IO) {
                NativeBridge.nativeParseInviteLink(raw)
            }
            val parsed = runCatching { JSONObject(parsedJson) }.getOrNull()
            if (parsed == null || parsed.has("error")) {
                val err = parsed?.optString("error") ?: "parse failed"
                emitMessage("Неправильный формат приглашения", UiSeverity.Error)
                Log.w("xr-onboarding", "parseInviteLink: $err")
                _onboardingState.value = initialOnboardingState()
                return@launch
            }
            val hubUrl = parsed.optString("hub_url")
            val token = parsed.optString("token")
            if (hubUrl.isBlank() || token.isBlank()) {
                emitMessage("Неправильный формат приглашения", UiSeverity.Error)
                _onboardingState.value = initialOnboardingState()
                return@launch
            }

            val infoJson = withContext(Dispatchers.IO) {
                NativeBridge.nativeFetchInviteInfo(hubUrl, token, 5_000L)
            }
            val info = runCatching { JSONObject(infoJson) }.getOrNull()
            if (info == null) {
                emitMessage("Ошибка ответа хаба", UiSeverity.Error)
                _onboardingState.value = initialOnboardingState()
                return@launch
            }
            if (info.has("error")) {
                emitMessage(friendlyInviteInfoError(info.optString("error")), UiSeverity.Error)
                _onboardingState.value = initialOnboardingState()
                return@launch
            }

            _onboardingState.value = OnboardingState.ConfirmInvite(
                hubUrl = hubUrl,
                token = token,
                preset = info.optString("preset"),
                comment = info.optString("comment"),
                status = info.optString("status", "active"),
                expiresAt = info.optString("expires_at"),
            )
        }
    }

    fun onInviteCancelled() {
        _onboardingState.value = initialOnboardingState()
    }

    fun onManualSetupChosen() {
        _onboardingState.value = OnboardingState.Completed
    }

    fun onInviteConfirmed() {
        val current = _onboardingState.value as? OnboardingState.ConfirmInvite ?: return
        if (current.applyInProgress) return
        if (_uiState.value.phase != ConnectPhase.Idle) {
            emitMessage("Сначала отключите VPN", UiSeverity.Warn)
            return
        }
        _onboardingState.value = current.copy(applyInProgress = true)

        viewModelScope.launch {
            val resultJson = withContext(Dispatchers.IO) {
                NativeBridge.nativeApplyInvite(
                    current.hubUrl, current.token, current.preset,
                    presetCacheDir.absolutePath, 5_000L,
                )
            }
            val result = runCatching { JSONObject(resultJson) }.getOrNull()
            val payload = result?.optJSONObject("payload")
            if (payload == null) {
                val errors = result?.optJSONArray("errors")
                val first = if (errors != null && errors.length() > 0) errors.optString(0) else "unknown"
                emitMessage(friendlyClaimError(first), UiSeverity.Error)
                _onboardingState.value = current.copy(applyInProgress = false)
                return@launch
            }

            val publicKey = result.optString("public_key").takeIf {
                it.isNotBlank() && it != "null"
            } ?: ""
            val presetCached = result.optBoolean("preset_cached", false)

            val hubFromPayload = payload.optString("hub_url").ifBlank { current.hubUrl }
            val serverAddr = payload.optString("server_address")
            val serverPort = payload.optInt("server_port", 8443)
            val presetName = payload.optString("preset")

            val profile = ServerProfile(
                id = UUID.randomUUID().toString(),
                name = repo.generateName(serverAddr, hubFromPayload, current.comment),
                serverAddress = serverAddr,
                serverPort = serverPort,
                obfuscationKey = payload.optString("obfuscation_key"),
                modifier = payload.optString("modifier", "positional_xor_rotate"),
                salt = payload.optLong("salt", 0xDEADBEEFL),
                routingPreset = presetName.ifBlank { "russia" },
                hubUrl = hubFromPayload,
                hubPreset = presetName,
                trustedPublicKey = publicKey,
                createdAt = OffsetDateTime.now().toString(),
                source = ServerSource.Invite,
            )
            repo.upsert(profile)
            repo.setActive(profile.id)

            if (!presetCached) {
                emitMessage("Хаб недоступен, подпись пресета не будет проверяться", UiSeverity.Warn)
            }
            _onboardingState.value = OnboardingState.Completed
        }
    }

    private fun friendlyInviteInfoError(code: String): String = when (code) {
        "not_found" -> "Приглашение не найдено"
        "gone" -> "Приглашение уже использовано или истекло"
        else -> when {
            code.startsWith("network") -> "Хаб недоступен. Проверьте интернет"
            code.contains("certificate") -> "Небезопасное соединение с хабом"
            code.startsWith("http_") -> "Ошибка хаба: ${code.removePrefix("http_")}"
            else -> "Ошибка: $code"
        }
    }

    private fun friendlyClaimError(code: String): String = when {
        code.contains("gone") -> "Приглашение уже использовано или истекло"
        code.contains("not_found") -> "Приглашение не найдено"
        code.contains("network") -> "Хаб недоступен. Проверьте интернет"
        code.contains("certificate") -> "Небезопасное соединение с хабом"
        else -> "Ошибка применения: $code"
    }

    private fun emitMessage(text: String, severity: UiSeverity) {
        viewModelScope.launch { _messages.emit(UiMessage(text, severity)) }
    }

    // ── VPN connection ──────────────────────────────────────────────

    fun onConnectClicked() {
        val s = _uiState.value
        if (s.phase != ConnectPhase.Idle) return
        val server = repo.activeServer()
        if (server == null || server.serverAddress.isBlank() || server.obfuscationKey.isBlank()) {
            emitMessage("Заполните сервер и ключ", UiSeverity.Info)
            return
        }

        _uiState.value = s.copy(phase = ConnectPhase.Preparing, state = "Connecting...")

        val intent: Intent? = try {
            VpnService.prepare(getApplication())
        } catch (_: Exception) { null }
        if (intent == null) {
            actuallyStart()
        } else {
            _uiState.value = _uiState.value.copy(phase = ConnectPhase.NeedsPermission)
            viewModelScope.launch { _permissionRequest.emit(intent) }
        }
    }

    fun onPermissionResult(granted: Boolean) {
        if (granted) {
            actuallyStart()
        } else {
            _uiState.value = _uiState.value.copy(phase = ConnectPhase.Idle, state = "Disconnected")
            emitMessage("VPN-разрешение не получено", UiSeverity.Info)
        }
    }

    private fun actuallyStart() {
        val server = repo.activeServer() ?: return
        val configJson = buildConfigJson(server)
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_START
            putExtra(XrVpnService.EXTRA_CONFIG_JSON, configJson)
        }
        getApplication<Application>().startForegroundService(intent)
        tryBind(autoCreate = true)
        _uiState.value = _uiState.value.copy(phase = ConnectPhase.Preparing, state = "Connecting...")
    }

    fun disconnect() {
        val svc = boundService
        if (svc != null) {
            svc.stopFromUi()
            return
        }
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_STOP
        }
        try { getApplication<Application>().startService(intent) } catch (_: Exception) {}
        _uiState.value = _uiState.value.copy(
            phase = ConnectPhase.Idle, state = "Disconnected",
            bytesUp = 0, bytesDown = 0, activeConnections = 0, uptime = 0,
            recentErrors = emptyList(),
        )
    }

    private fun applyServiceState(svcState: XrVpnService.ServiceState) {
        val phase = when (svcState.phase) {
            XrVpnService.Phase.Idle -> ConnectPhase.Idle
            XrVpnService.Phase.Preparing -> ConnectPhase.Preparing
            XrVpnService.Phase.Connecting -> ConnectPhase.Connecting
            XrVpnService.Phase.Finalizing -> ConnectPhase.Finalizing
            XrVpnService.Phase.Connected -> ConnectPhase.Connected
            XrVpnService.Phase.Stopping -> ConnectPhase.Stopping
            XrVpnService.Phase.Error -> ConnectPhase.Idle
        }
        val stateStr = when (svcState.phase) {
            XrVpnService.Phase.Idle -> "Disconnected"
            XrVpnService.Phase.Preparing -> "Preparing..."
            XrVpnService.Phase.Connecting -> "Connecting..."
            XrVpnService.Phase.Finalizing -> "Finalizing..."
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
            speedUp = svcState.speedUp,
            speedDown = svcState.speedDown,
            health = svcState.health,
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
            emitMessage(svcState.errorMessage, UiSeverity.Error)
        }
        if ((svcState.phase == XrVpnService.Phase.Idle ||
                    svcState.phase == XrVpnService.Phase.Error) && isBound
        ) {
            viewModelScope.launch { unbindAndClear() }
        }
    }

    // ── Config building ─────────────────────────────────────────────

    private fun buildConfigJson(server: ServerProfile): String {
        val routingToml = buildRoutingToml(server)
        val systemDns = collectSystemDnsServers()
        val dnsArray = systemDns.joinToString(", ") { "\"$it\"" }
        val hubFields = if (server.hubUrl.isNotBlank() && server.hubPreset.isNotBlank()) {
            """,
                "hub_url": "${server.hubUrl}",
                "hub_preset": "${server.hubPreset}",
                "hub_cache_dir": "${presetCacheDir.absolutePath}",
                "hub_refresh_interval_secs": 300"""
        } else ""
        return """
            {
                "server_address": "${server.serverAddress}",
                "server_port": ${server.serverPort},
                "obfuscation_key": "${server.obfuscationKey}",
                "modifier": "${server.modifier}",
                "salt": ${server.salt},
                "padding_min": 16,
                "padding_max": 128,
                "routing_toml": "${routingToml.replace("\"", "\\\"").replace("\n", "\\n")}",
                "on_server_down": "direct",
                "dns_resolvers": [$dnsArray]$hubFields
            }
        """.trimIndent()
    }

    private fun collectSystemDnsServers(): List<String> {
        val cm = getApplication<Application>()
            .getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            ?: return emptyList()
        val seen = mutableSetOf<String>()
        val result = mutableListOf<String>()
        fun addFrom(network: Network?) {
            if (network == null) return
            val lp: LinkProperties = cm.getLinkProperties(network) ?: return
            for (addr in lp.dnsServers) {
                val ip = addr.hostAddress ?: continue
                if (ip.startsWith("10.0.0.") || ip == "127.0.0.1" || ip.startsWith("169.254.")) continue
                if (seen.add(ip)) result.add(ip)
            }
        }
        addFrom(cm.activeNetwork)
        return result
    }

    private fun buildRoutingToml(server: ServerProfile): String = when (server.routingPreset) {
        "proxy_all" -> "default_action = \"proxy\"\n"
        "russia" -> PRESET_RUSSIA
        "custom" -> buildCustomRoutingToml(server)
        else -> PRESET_RUSSIA
    }

    private fun buildCustomRoutingToml(server: ServerProfile): String {
        val sb = StringBuilder()
        sb.appendLine("default_action = \"direct\"")
        val domains = server.customDomains.lines().map { it.trim() }.filter { it.isNotBlank() }
        val ipRanges = server.customIpRanges.lines().map { it.trim() }.filter { it.isNotBlank() }
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

        fun parseTomlDomains(toml: String): Pair<List<String>, List<String>> {
            val domains = mutableListOf<String>()
            val ipRanges = mutableListOf<String>()
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
            return domains to ipRanges
        }
    }
}
