package com.xrproxy.app.ui

import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.SharedPreferences
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.VpnService
import android.os.IBinder
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
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
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File

/** High-level phase of the VPN session as seen by the UI (LLD-06 §3.6). */
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

/** Snackbar message with severity for styled XrSnackbar (LLD-06 §3.9a). */
enum class UiSeverity { Info, Warn, Error }
data class UiMessage(val text: String, val severity: UiSeverity = UiSeverity.Info)

/**
 * Onboarding flow state (LLD-04 §3.8). `Completed` = основной UI; всё
 * остальное — pre-main экраны, в которых нижний NavigationBar скрыт.
 */
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
    // Speed (bytes/sec)
    val speedUp: Long = 0,
    val speedDown: Long = 0,
    // Health
    val health: HealthLevel = HealthLevel.Healthy,
    // Debug
    val dnsQueries: Long = 0,
    val tcpSyns: Long = 0,
    val smolRecv: Long = 0,
    val smolSend: Long = 0,
    val relayWarnings: Long = 0,
    val relayErrors: Long = 0,
    val debugMsg: String = "",
    val recentErrors: List<String> = emptyList(),
    val debugExpanded: Boolean = false,
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
    // Hub (LLD-04 onboarding, populated after Apply)
    val hubUrl: String = "",
    val hubPreset: String = "",
    val trustedPublicKey: String = "",
    // UI feedback
    val settingsSaved: Boolean = false,
) {
    val connected: Boolean
        get() = phase == ConnectPhase.Connected
    val connecting: Boolean
        get() = phase.isTransitioning
}

class VpnViewModel(application: Application) : AndroidViewModel(application) {

    private val prefs: SharedPreferences =
        application.getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)

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
        _onboardingState.value = initialOnboardingState()
        // Best-effort bind: if the service is already running (app re-entered
        // from background), we immediately mirror its state. If it isn't, we
        // stay Idle and bind again when the user hits Connect.
        tryBind(autoCreate = false)
    }

    /**
     * Welcome отображается, пока нет ни ручной конфигурации (server_address)
     * ни хаба (hub_url). Любое из двух — считаем настройку завершённой.
     */
    private fun initialOnboardingState(): OnboardingState {
        val s = _uiState.value
        return if (s.serverAddress.isBlank() && s.hubUrl.isBlank()) {
            OnboardingState.ShowingWelcome
        } else {
            OnboardingState.Completed
        }
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
            hubUrl = prefs.getString("hub_url", "") ?: "",
            hubPreset = prefs.getString("hub_preset", "") ?: "",
            trustedPublicKey = prefs.getString("trusted_public_key", "") ?: "",
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

    fun toggleDebug() {
        _uiState.value = _uiState.value.copy(debugExpanded = !_uiState.value.debugExpanded)
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

    // ── Onboarding (LLD-04) ─────────────────────────────────────────

    /**
     * Вход для трёх точек: отсканированный QR, вставленная ссылка, deep link.
     * Парсим URL → GET invite info → выставляем ConfirmInvite или Error
     * через Snackbar. Invite в этот момент **не** consume'ится.
     */
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
                val err = info.optString("error")
                emitMessage(friendlyInviteInfoError(err), UiSeverity.Error)
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

    /** Welcome → Settings (ручная настройка). Создаём «заглушку» в prefs —
     *  серверного адреса ещё нет, но пользователь попадает в главный UI. */
    fun onManualSetupChosen() {
        _onboardingState.value = OnboardingState.Completed
    }

    /**
     * Фаза 2: claim + TOFU public-key + pre-warm preset. Вызывается по
     * нажатию «Применить» на экране подтверждения.
     */
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
                    current.hubUrl,
                    current.token,
                    current.preset,
                    presetCacheDir.absolutePath,
                    5_000L,
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

            persistApplyResult(payload, current.hubUrl, publicKey)

            if (!presetCached) {
                emitMessage(
                    "Хаб недоступен, подпись пресета не будет проверяться",
                    UiSeverity.Warn,
                )
            }
            _onboardingState.value = OnboardingState.Completed
        }
    }

    private fun persistApplyResult(payload: JSONObject, hubUrl: String, publicKey: String) {
        val serverAddress = payload.optString("server_address")
        val serverPort = payload.optInt("server_port", 8443).toString()
        val obfKey = payload.optString("obfuscation_key")
        val modifier = payload.optString("modifier", "positional_xor_rotate")
        val salt = payload.optLong("salt", 0xDEADBEEFL).toString()
        val presetName = payload.optString("preset")
        // Payload хранит собственный hub_url — используем его, а не тот, по
        // которому пришла ссылка, чтобы хаб мог направить клиента на canonical
        // адрес (например, при смене домена).
        val hubFromPayload = payload.optString("hub_url").ifBlank { hubUrl }

        prefs.edit()
            .putString("server_address", serverAddress)
            .putString("server_port", serverPort)
            .putString("obfuscation_key", obfKey)
            .putString("modifier", modifier)
            .putString("salt", salt)
            .putString("routing_preset", presetName.ifBlank { "russia" })
            .putString("hub_url", hubFromPayload)
            .putString("hub_preset", presetName)
            .putString("trusted_public_key", publicKey)
            .apply()

        _uiState.value = _uiState.value.copy(
            serverAddress = serverAddress,
            serverPort = serverPort,
            obfuscationKey = obfKey,
            modifier = modifier,
            salt = salt,
            routingPreset = presetName.ifBlank { "russia" },
            hubUrl = hubFromPayload,
            hubPreset = presetName,
            trustedPublicKey = publicKey,
        )
    }

    private fun friendlyInviteInfoError(code: String): String = when (code) {
        "not_found" -> "Приглашение не найдено"
        "gone" -> "Приглашение уже использовано или истекло"
        else -> when {
            code.startsWith("network") -> "Хаб недоступен. Проверьте интернет"
            code.contains("invalid certificate") || code.contains("certificate") ->
                "Небезопасное соединение с хабом"
            code.startsWith("http_4") || code.startsWith("http_5") ->
                "Ошибка хаба: ${code.removePrefix("http_")}"
            else -> "Ошибка: $code"
        }
    }

    private fun friendlyClaimError(code: String): String = when {
        code.contains("gone") -> "Приглашение уже использовано или истекло"
        code.contains("not_found") -> "Приглашение не найдено"
        code.startsWith("claim network") || code.startsWith("network") ->
            "Хаб недоступен. Проверьте интернет"
        code.contains("certificate") -> "Небезопасное соединение с хабом"
        else -> "Ошибка применения: $code"
    }

    private fun emitMessage(text: String, severity: UiSeverity) {
        viewModelScope.launch { _messages.emit(UiMessage(text, severity)) }
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
            viewModelScope.launch { _messages.emit(UiMessage("Заполните сервер и ключ в Settings", UiSeverity.Info)) }
            return
        }

        _uiState.value = s.copy(phase = ConnectPhase.Preparing, state = "Connecting...")

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
            viewModelScope.launch { _messages.emit(UiMessage("VPN-разрешение не получено", UiSeverity.Info)) }
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
        _uiState.value = _uiState.value.copy(phase = ConnectPhase.Preparing, state = "Connecting...")
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
            viewModelScope.launch { _messages.emit(UiMessage(svcState.errorMessage, UiSeverity.Error)) }
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
        // System DNS comes from the ACTIVE network we have BEFORE the VPN
        // takes over — once the TUN is up, getActiveNetwork() returns the
        // VPN itself and its DNS would be 10.0.0.1 (FakeDNS), creating a
        // resolution loop. Сбор делаем тут, до запуска сервиса.
        val systemDns = collectSystemDnsServers()
        val dnsArray = systemDns.joinToString(", ") { "\"$it\"" }
        // Hub-поля опциональны: если инвайт ещё не применялся, движок их не
        // увидит и будет работать на локальном routing_toml. Если применялся —
        // движок включит PresetCache и начнёт периодический sanity-check.
        val hubFields = if (state.hubUrl.isNotBlank() && state.hubPreset.isNotBlank()) {
            """,
                "hub_url": "${state.hubUrl}",
                "hub_preset": "${state.hubPreset}",
                "hub_cache_dir": "${presetCacheDir.absolutePath}",
                "hub_refresh_interval_secs": 300"""
        } else {
            ""
        }
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
                "on_server_down": "direct",
                "dns_resolvers": [$dnsArray]$hubFields
            }
        """.trimIndent()
    }

    /**
     * Returns DNS servers from the currently active (non-VPN) network.
     * Used to give the engine a working resolver path through carrier-imposed
     * whitelists where public resolvers (8.8.8.8, 1.1.1.1) might be blocked.
     */
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
                // Skip VPN / loopback / link-local addresses.
                if (ip.startsWith("10.0.0.") || ip == "127.0.0.1" || ip.startsWith("169.254.")) continue
                if (seen.add(ip)) result.add(ip)
            }
        }

        // activeNetwork в момент ДО старта VPN — это реальная сеть (Wi-Fi
        // или мобильная). Этого достаточно: после старта VPN эта же сеть
        // остаётся под капотом, а DNS её по-прежнему применимы. Раньше
        // дополнительно обходили cm.allNetworks, чтобы подобрать Wi-Fi DNS
        // когда активна мобильная, но allNetworks deprecated с API 31, а
        // сценарий не принципиален: fallback на public DNS (1.1.1.1/8.8.8.8)
        // всё равно срабатывает в race вместе с активным резолвером.
        addFrom(cm.activeNetwork)

        return result
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
