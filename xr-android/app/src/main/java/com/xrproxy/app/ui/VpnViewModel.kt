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
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.data.ProfileEndpoint
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.data.ServerRepository
import com.xrproxy.app.data.ServerSource
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.HealthLevel
import com.xrproxy.app.service.XrVpnService
import com.xrproxy.app.update.UpdateManager
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
    Paused,
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

/** APK self-update UI state (LLD-12 §2.3). */
sealed interface UpdateUiState {
    object Idle : UpdateUiState
    object Checking : UpdateUiState
    /** Transient: shown only after a *manual* check that found nothing newer. */
    object UpToDate : UpdateUiState
    data class Available(val release: UpdateManager.Release) : UpdateUiState
    data class Downloading(val release: UpdateManager.Release, val progress: Float) : UpdateUiState
    data class ReadyToInstall(val release: UpdateManager.Release, val file: java.io.File) : UpdateUiState
    /** The system installer has been launched for [file]; the in-app banner
     *  hides while the OS confirm dialog is up. Carries enough to fall back to
     *  [ReadyToInstall] (offer "Установить" again) if the user dismisses it. */
    data class Installing(val release: UpdateManager.Release, val file: java.io.File) : UpdateUiState
    data class Error(val message: String) : UpdateUiState
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
    /** SSID of the trusted network the tunnel is paused on, when [phase] is Paused. */
    val pausedSsid: String? = null,
    /** While paused: this trusted network failed the restriction probe (task 3b-2 §2). */
    val restrictedNetwork: Boolean = false,
    /** Log tab search query (LLD-03). Lives in VM so it survives tab switches. */
    val logQuery: String = "",
    val logRegexMode: Boolean = false,
    /** Имя активного сервера пула (LLD-10); пустое, пока движок не запущен. */
    val activeServer: String = "",
    /** Активен резерв, статусная строка показывает «через X (резерв)». */
    val backupActive: Boolean = false,
) {
    val connected: Boolean
        get() = phase == ConnectPhase.Connected
    val connecting: Boolean
        get() = phase.isTransitioning
    val paused: Boolean
        get() = phase == ConnectPhase.Paused
}

class VpnViewModel(application: Application) : AndroidViewModel(application) {

    private val prefs = application.getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)

    val repo = ServerRepository(prefs)

    val trustedRepo = com.xrproxy.app.data.TrustedNetworksRepository(prefs)

    private val _uiState = MutableStateFlow(VpnUiState())
    val uiState: StateFlow<VpnUiState> = _uiState

    private val _onboardingState = MutableStateFlow<OnboardingState>(OnboardingState.Loading)
    val onboardingState: StateFlow<OnboardingState> = _onboardingState

    private val _permissionRequest = MutableSharedFlow<Intent>(extraBufferCapacity = 1)
    val permissionRequest: SharedFlow<Intent> = _permissionRequest

    private val _messages = MutableSharedFlow<UiMessage>(extraBufferCapacity = 4)
    val messages: SharedFlow<UiMessage> = _messages

    // ── APK self-update (LLD-12) ────────────────────────────────────
    private val updateManager = UpdateManager(application)

    private val _updateState = MutableStateFlow<UpdateUiState>(UpdateUiState.Idle)
    val updateState: StateFlow<UpdateUiState> = _updateState

    // Small de-dup window between *automatic* checks, NOT a throttle: the
    // triggers are already rare key events (app brought to foreground, fresh
    // connect), so we check on each one. This only coalesces a near-simultaneous
    // double-fire (e.g. foreground + auto-connect on open). Manual checks bypass
    // it. A deliberate re-open minutes later still checks — that was the bug with
    // the old multi-hour floor (it ate the very event the user cares about).
    private val autoUpdateCheckDedupMs = 60L * 1000
    private val keyLastUpdateCheck = "last_update_check_ms"
    // One background check at a time: it retries with backoff, so a second
    // trigger arriving mid-retry must not launch a parallel run (XR-024).
    @Volatile private var updateCheckInFlight = false

    // Checks for updates on a real app foreground (background→foreground) — the
    // key "user opened the app" event, fired once per transition, NOT on rotation
    // or internal navigation (unlike Activity.onStart). Registering while the app
    // is already STARTED delivers onStart immediately, so the initial open is
    // covered too. Removed in onCleared.
    private val foregroundObserver = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            checkForUpdates(manual = false)
            // Re-run the restriction probe when the user opens the app while
            // paused, so a stale "network restricted" warning doesn't linger
            // until the next periodic re-probe.
            boundService?.reprobeRestrictionsIfPaused()
            // Re-evaluate the trusted-network decision: while the device is idle
            // the auto-pause can be missed (network callbacks coalesced in Doze,
            // the service poll-loop frozen with the CPU asleep), so the tunnel
            // can sit up on a trusted Wi-Fi until the app is opened. Doing it
            // here makes opening the app deterministically land the pause.
            boundService?.reevaluateTrustedNetwork()
        }
    }

    /** Intents the Activity should `startActivity` (e.g. the "allow install
     *  from this source" system screen). One-shot, like [permissionRequest]. */
    private val _openIntent = MutableSharedFlow<Intent>(extraBufferCapacity = 1)
    val openIntent: SharedFlow<Intent> = _openIntent

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
        updateManager.onInstallStatus = { status ->
            when (status) {
                is UpdateManager.InstallStatus.Success -> {
                    emitMessage("Обновление установлено", UiSeverity.Info)
                    _updateState.value = UpdateUiState.Idle
                }
                is UpdateManager.InstallStatus.Cancelled -> {
                    // User dismissed the system installer (no error). Fall back
                    // to the ready banner so "Установить" is offered again — both
                    // now and on the next launch (the verified APK is cached).
                    (_updateState.value as? UpdateUiState.Installing)?.let {
                        _updateState.value = UpdateUiState.ReadyToInstall(it.release, it.file)
                    }
                }
                is UpdateManager.InstallStatus.Failed -> {
                    emitMessage("Установка не удалась: ${status.message}", UiSeverity.Error)
                    _updateState.value = UpdateUiState.Error("install: ${status.message}")
                }
            }
        }
        // Проверка обновлений — событийная, на КЛЮЧЕВЫЕ события: выход
        // приложения на передний план (ProcessLifecycleOwner, реальный
        // background→foreground) и свежий переход в Connected (applyServiceState).
        // Оба редкие, поэтому без большого пола — только 60с дедуп от двойного
        // срабатывания. addObserver при уже STARTED сразу дёргает onStart →
        // первый открыв тоже покрыт.
        ProcessLifecycleOwner.get().lifecycle.addObserver(foregroundObserver)
    }

    private fun initialOnboardingState(): OnboardingState =
        if (repo.servers.value.isEmpty()) OnboardingState.ShowingWelcome
        else OnboardingState.Completed

    override fun onCleared() {
        ProcessLifecycleOwner.get().lifecycle.removeObserver(foregroundObserver)
        serviceObserverJob?.cancel()
        if (isBound) {
            try { getApplication<Application>().unbindService(bindConnection) } catch (_: Exception) {}
            isBound = false
        }
        updateManager.release()
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

    // ── Log tab (LLD-03) ────────────────────────────────────────────

    fun updateLogQuery(q: String) {
        _uiState.value = _uiState.value.copy(logQuery = q)
    }

    fun toggleLogRegexMode() {
        _uiState.value = _uiState.value.copy(logRegexMode = !_uiState.value.logRegexMode)
    }

    /** Full, unfiltered log — toolbar actions always operate on this, the
     *  search field is only a visual filter (LLD-03 §2.4). */
    private fun buildFullLog(): String = _uiState.value.recentErrors.joinToString("\n")

    fun copyLog() {
        val cm = getApplication<Application>()
            .getSystemService(Context.CLIPBOARD_SERVICE) as? android.content.ClipboardManager
        if (cm == null) {
            emitMessage("Буфер обмена недоступен", UiSeverity.Warn)
            return
        }
        cm.setPrimaryClip(android.content.ClipData.newPlainText("xr-proxy log", buildFullLog()))
        emitMessage("Скопировано", UiSeverity.Info)
    }

    fun shareLog(context: Context) {
        try {
            val file = File(context.cacheDir, "xr-proxy.log")
            file.writeText(buildFullLog())
            val uri = androidx.core.content.FileProvider.getUriForFile(
                context, "${context.packageName}.fileprovider", file,
            )
            val intent = Intent(Intent.ACTION_SEND).apply {
                type = "text/plain"
                putExtra(Intent.EXTRA_STREAM, uri)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            context.startActivity(Intent.createChooser(intent, "Share log"))
        } catch (e: Exception) {
            emitMessage("Не удалось поделиться: ${e.message}", UiSeverity.Error)
        }
    }

    /** Write the full log to a user-chosen SAF document (LLD-03 §3.5). */
    fun writeLogTo(uri: android.net.Uri, resolver: android.content.ContentResolver) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                resolver.openOutputStream(uri)?.use { out ->
                    out.writer(Charsets.UTF_8).use { w ->
                        _uiState.value.recentErrors.forEach { line ->
                            w.write(line); w.write("\n")
                        }
                    }
                }
                emitMessage("Лог сохранён", UiSeverity.Info)
            } catch (e: Exception) {
                emitMessage("Не удалось сохранить: ${e.message}", UiSeverity.Error)
            }
        }
    }

    // ── Trusted networks / auto-pause (task 3b-2) ───────────────────

    fun addTrustedNetwork(ssid: String) {
        val clean = ssid.trim()
        if (clean.isBlank()) {
            emitMessage("Введите имя сети (SSID)", UiSeverity.Info)
            return
        }
        trustedRepo.add(clean)
    }

    fun removeTrustedNetwork(ssid: String) = trustedRepo.remove(ssid)

    fun setTrustedAutoPauseEnabled(enabled: Boolean) = trustedRepo.setEnabled(enabled)

    /**
     * Best-effort current Wi-Fi SSID for the "add current network" shortcut.
     * Prefers the running service's non-redacted value; otherwise queries the
     * active network's capabilities (which may be redacted to "<unknown ssid>"
     * without location permission — returns null then, and the user types it
     * manually). Normalized through the Rust bridge.
     */
    fun suggestCurrentSsid(): String? {
        val fromService = boundService?.currentRawSsidOrNull()
        val raw = fromService ?: run {
            val cm = getApplication<Application>()
                .getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
                ?: return null
            val net = cm.activeNetwork ?: return null
            val caps = cm.getNetworkCapabilities(net) ?: return null
            val info = caps.transportInfo
            if (info is android.net.wifi.WifiInfo) info.ssid else null
        } ?: return null
        return NativeBridge.nativeNormalizeSsid(raw)
    }

    /**
     * Best-effort list of nearby Wi-Fi SSIDs for the "add network" picker:
     * the current network first, then cached scan results. Empty when location
     * permission/services are off (the picker falls back to manual entry).
     * Uses cached scanResults (no startScan) to avoid scan throttling.
     */
    @Suppress("DEPRECATION")
    fun availableSsids(): List<String> {
        val out = LinkedHashSet<String>()
        suggestCurrentSsid()?.let { out.add(it) }
        val wifi = getApplication<Application>()
            .getSystemService(Context.WIFI_SERVICE) as? android.net.wifi.WifiManager
        if (wifi != null) {
            try {
                for (sr in wifi.scanResults) {
                    val raw = sr.SSID
                    if (raw.isNullOrBlank()) continue
                    NativeBridge.nativeNormalizeSsid(raw)?.let { out.add(it) }
                }
            } catch (_: SecurityException) {
                // No location permission — leave whatever we have.
            } catch (_: Exception) {
                // OEM quirks — ignore, manual entry still works.
            }
        }
        return out.toList()
    }

    /** Keep the tunnel running on the current trusted network ("Включить здесь"). */
    fun resumeOnTrustedNetwork() {
        boundService?.resumeOnTrustedNetwork()
    }

    // ── APK self-update (LLD-12) ────────────────────────────────────

    /** Hub of the active server, or null when none is configured. */
    private fun activeHubUrl(): String? =
        repo.activeServer()?.hubUrl?.takeIf { it.isNotBlank() }

    /**
     * Check the hub for a newer signed release. [manual] checks surface
     * "up to date" / errors to the user; background checks stay silent on
     * failure and only pop the banner when something newer is verified.
     */
    fun checkForUpdates(manual: Boolean) {
        val hubUrl = activeHubUrl()
        if (hubUrl == null) {
            if (manual) emitMessage("Для проверки обновлений нужен сервер с хабом", UiSeverity.Info)
            return
        }
        // Never interrupt an in-flight download / install.
        when (_updateState.value) {
            is UpdateUiState.Downloading, is UpdateUiState.ReadyToInstall,
            is UpdateUiState.Installing -> return
            else -> {}
        }
        // A background check already retrying covers this trigger.
        if (!manual && updateCheckInFlight) return
        // Rate-limit only CONCLUSIVE background checks (60s) so a burst of events
        // doesn't spam the hub. A FAILED attempt must not count: the cold-start
        // trigger often fires before connectivity is up, and stamping on a failure
        // used to poison the window so the banner never appeared until much later
        // (XR-024). So we stamp on Available/UpToDate only, inside the loop.
        val now = System.currentTimeMillis()
        if (!manual && now - prefs.getLong(keyLastUpdateCheck, 0L) < autoUpdateCheckDedupMs) return
        // Spinner only for an explicit user check; the launch check is silent.
        if (manual) _updateState.value = UpdateUiState.Checking
        viewModelScope.launch {
            updateCheckInFlight = true
            try {
                // Background checks retry with backoff: a single silent failure on
                // a not-yet-ready network used to leave the user without the banner
                // until a later trigger happened to succeed.
                val backoffMs = longArrayOf(0L, 4_000L, 12_000L, 30_000L)
                val attempts = if (manual) 1 else backoffMs.size
                for (i in 0 until attempts) {
                    if (i > 0) delay(backoffMs[i])
                    val result = withContext(Dispatchers.IO) { updateManager.check(hubUrl) }
                    when (result) {
                        is UpdateManager.CheckResult.Available -> {
                            prefs.edit().putLong(keyLastUpdateCheck, System.currentTimeMillis()).apply()
                            // If this APK was already downloaded and verified in a
                            // prior session, offer "Установить" directly instead of
                            // re-downloading. Re-hashing the cached file stays on IO.
                            val cached = withContext(Dispatchers.IO) {
                                updateManager.cachedVerifiedApk(result.release)
                            }
                            _updateState.value = if (cached != null)
                                UpdateUiState.ReadyToInstall(result.release, cached)
                            else
                                UpdateUiState.Available(result.release)
                            return@launch
                        }
                        is UpdateManager.CheckResult.UpToDate -> {
                            prefs.edit().putLong(keyLastUpdateCheck, System.currentTimeMillis()).apply()
                            _updateState.value =
                                if (manual) UpdateUiState.UpToDate else UpdateUiState.Idle
                            return@launch
                        }
                        is UpdateManager.CheckResult.Failed -> {
                            // Manual: surface the error now. Background: keep retrying
                            // and do NOT stamp the rate-limit, so the next trigger is
                            // free to try too.
                            if (manual) {
                                _updateState.value =
                                    UpdateUiState.Error(friendlyUpdateError(result.error))
                                return@launch
                            }
                        }
                    }
                }
            } finally {
                updateCheckInFlight = false
            }
        }
    }

    /** Download + Rust-verify the available release, then hand off to install. */
    fun startUpdateDownload() {
        val release = (_updateState.value as? UpdateUiState.Available)?.release ?: return
        _updateState.value = UpdateUiState.Downloading(release, 0f)
        viewModelScope.launch {
            try {
                val file = withContext(Dispatchers.IO) {
                    updateManager.download(release) { p ->
                        _updateState.value = UpdateUiState.Downloading(release, p)
                    }
                }
                _updateState.value = UpdateUiState.ReadyToInstall(release, file)
                installReadyUpdate()
            } catch (e: Exception) {
                _updateState.value =
                    UpdateUiState.Error(friendlyUpdateError(e.message ?: "download"))
            }
        }
    }

    /** Launch the system installer for the verified APK. If install-from-this
     *  source isn't granted yet, lead the user to the system screen first. */
    fun installReadyUpdate() {
        val s = _updateState.value as? UpdateUiState.ReadyToInstall ?: return
        if (!updateManager.canRequestInstall()) {
            emitMessage(
                "Разрешите установку из этого источника, затем нажмите «Установить»",
                UiSeverity.Info,
            )
            viewModelScope.launch { _openIntent.emit(updateManager.unknownSourcesSettingsIntent()) }
            return
        }
        // Hide the in-app banner while the system installer is up: the OS shows
        // its own confirm dialog, so a duplicate "Установить" card is confusing.
        // If the user dismisses that dialog we drop back to ReadyToInstall.
        _updateState.value = UpdateUiState.Installing(s.release, s.file)
        // The PackageInstaller session copies the (multi-MB) APK — keep it off
        // the main thread. The system confirm dialog is launched later from the
        // install-result receiver, so nothing UI-blocking happens here.
        viewModelScope.launch { withContext(Dispatchers.IO) { updateManager.install(s.file) } }
    }

    fun dismissUpdate() {
        _updateState.value = UpdateUiState.Idle
    }

    private fun friendlyUpdateError(code: String): String = when {
        code == "no_release" -> "На хабе пока нет опубликованных релизов"
        code == "no_hub" -> "Для сервера не задан хаб"
        code == "no_release_key" -> "В этой сборке обновление по воздуху отключено"
        code == "sha_mismatch" -> "Загруженный файл повреждён — попробуйте ещё раз"
        code.startsWith("verify") -> "Подпись обновления неверна — установка отклонена"
        code.startsWith("network") || code.startsWith("http") ->
            "Хаб недоступен. Проверьте интернет"
        else -> "Не удалось обновить: $code"
    }

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
            // Пул серверов из payload'а (LLD-10 §2.8); легаси-инвайт без
            // `servers` даёт профиль с одним адресом, как раньше.
            val endpoints = parsePayloadServers(payload).ifEmpty {
                if (serverAddr.isBlank()) emptyList()
                else listOf(ProfileEndpoint(address = serverAddr, port = serverPort))
            }

            val profile = ServerProfile(
                id = UUID.randomUUID().toString(),
                name = repo.generateName(serverAddr, hubFromPayload, current.comment),
                serverAddress = endpoints.firstOrNull()?.address ?: serverAddr,
                serverPort = endpoints.firstOrNull()?.port ?: serverPort,
                endpoints = endpoints,
                obfuscationKey = payload.optString("obfuscation_key"),
                modifier = payload.optString("modifier", "positional_xor_rotate"),
                salt = payload.optLong("salt", 0xDEADBEEFL),
                routingPreset = presetName.ifBlank { "russia" },
                hubUrl = hubFromPayload,
                hubPreset = presetName,
                trustedPublicKey = publicKey,
                inviteToken = current.token,
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

    /** Список серверов инвайт-payload'а, отсортированный по `priority`. */
    private fun parsePayloadServers(payload: JSONObject): List<ProfileEndpoint> {
        val arr = payload.optJSONArray("servers") ?: return emptyList()
        return (0 until arr.length())
            .mapNotNull { i -> arr.optJSONObject(i) }
            .filter { it.optString("address").isNotBlank() }
            .sortedBy { it.optInt("priority", 0) }
            .map {
                ProfileEndpoint(
                    name = it.optString("name", ""),
                    address = it.optString("address"),
                    port = it.optInt("port", 8443),
                )
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
        if (server == null || server.effectiveEndpoints.isEmpty() || server.obfuscationKey.isBlank()) {
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
        val prevPhase = _uiState.value.phase
        val phase = when (svcState.phase) {
            XrVpnService.Phase.Idle -> ConnectPhase.Idle
            XrVpnService.Phase.Preparing -> ConnectPhase.Preparing
            XrVpnService.Phase.Connecting -> ConnectPhase.Connecting
            XrVpnService.Phase.Finalizing -> ConnectPhase.Finalizing
            XrVpnService.Phase.Connected -> ConnectPhase.Connected
            XrVpnService.Phase.Paused -> ConnectPhase.Paused
            XrVpnService.Phase.Stopping -> ConnectPhase.Stopping
            XrVpnService.Phase.Error -> ConnectPhase.Idle
        }
        val stateStr = when (svcState.phase) {
            XrVpnService.Phase.Idle -> "Disconnected"
            XrVpnService.Phase.Preparing -> "Preparing..."
            XrVpnService.Phase.Connecting -> "Connecting..."
            XrVpnService.Phase.Finalizing -> "Finalizing..."
            XrVpnService.Phase.Connected -> "Connected"
            XrVpnService.Phase.Paused -> "Paused"
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
            // Native engine errors + restriction-probe diagnostics (the latter
            // run while paused with the engine down, so they ride in svcState).
            recentErrors = (snap?.recentErrors ?: emptyList()) + svcState.probeLog,
            pausedSsid = svcState.pausedSsid,
            restrictedNetwork = svcState.restrictedNetwork,
            activeServer = snap?.activeServer ?: "",
            backupActive = snap?.backupActive ?: false,
        )
        if (svcState.phase == XrVpnService.Phase.Error && svcState.errorMessage != null) {
            emitMessage(svcState.errorMessage, UiSeverity.Error)
        }
        if ((svcState.phase == XrVpnService.Phase.Idle ||
                    svcState.phase == XrVpnService.Phase.Error) && isBound
        ) {
            viewModelScope.launch { unbindAndClear() }
        }
        // A fresh connect is a good moment to check for updates (known-good
        // network); rate-limited like the foreground check.
        if (phase == ConnectPhase.Connected && prevPhase != ConnectPhase.Connected) {
            checkForUpdates(manual = false)
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
        // Пул серверов (LLD-10): порядок в массиве и есть приоритет.
        // Legacy-поля с primary остаются рядом, движок и старые версии
        // читают их как раньше.
        val endpoints = server.effectiveEndpoints
        // Пустой пул сюда не доходит (onConnectClicked валидирует), но пустой
        // адрес пусть отвергает движок своей ошибкой, а не NPE здесь.
        val primary = endpoints.firstOrNull()
            ?: ProfileEndpoint(address = server.serverAddress, port = server.serverPort)
        fun esc(s: String) = s.replace("\\", "\\\\").replace("\"", "\\\"")
        val serversArray = endpoints.joinToString(", ") {
            """{"name": "${esc(it.name)}", "address": "${esc(it.address)}", "port": ${it.port}}"""
        }
        return """
            {
                "server_address": "${esc(primary.address)}",
                "server_port": ${primary.port},
                "servers": [$serversArray],
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
