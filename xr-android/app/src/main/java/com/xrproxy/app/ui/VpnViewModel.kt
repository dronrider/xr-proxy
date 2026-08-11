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
import com.xrproxy.app.R
import com.xrproxy.app.data.CachedPreset
import com.xrproxy.app.data.JournalSettings
import com.xrproxy.app.data.PresetCacheReader
import com.xrproxy.app.data.ProfileEndpoint
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.data.ServerRepository
import com.xrproxy.app.data.ServerSource
import com.xrproxy.app.data.UserRule
import com.xrproxy.app.data.UserRulesStore
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
import org.json.JSONArray
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

    /** Ожидание. [hubUrl] пуст на старте приложения и на миг локального
     *  разбора ссылки; как только адрес известен, экран говорит, к какому
     *  хабу идёт запрос (XR-234). */
    data class Loading(val hubUrl: String = "") : OnboardingState

    /** Отказ проверки инвайта. Живёт экраном, а не снекбаром: причина
     *  остаётся перед глазами, а [rawLink] даёт «Повторить» без повторной
     *  вставки ссылки. [retryable] ложен, когда дело в самом приглашении
     *  и повтор бессмыслен (XR-234). */
    data class InviteError(
        val hubUrl: String,
        val title: String,
        val detail: String,
        val retryable: Boolean,
        val rawLink: String,
    ) : OnboardingState
    data class ConfirmInvite(
        val hubUrl: String,
        val token: String,
        val preset: String,
        val comment: String,
        val status: String,
        val expiresAt: String,
        /** Инвайт потреблён, но потребила его эта же установка: повторное
         *  применение пройдёт по ключу из кэша (XR-216). */
        val reclaimable: Boolean = false,
        val applyInProgress: Boolean = false,
    ) : OnboardingState
    object Completed : OnboardingState
}

/** APK self-update UI state (LLD-12 §2.3). */
sealed interface UpdateUiState {
    object Idle : UpdateUiState
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

/** Есть непоставленное обновление: пока оно висит, на иконке вкладки
 *  «Серверы» горит точка, чтобы индикация была видна с любой вкладки (XR-041). */
val UpdateUiState.updatePending: Boolean
    get() = this is UpdateUiState.Available || this is UpdateUiState.Downloading ||
        this is UpdateUiState.ReadyToInstall || this is UpdateUiState.Installing

/** Исход «Обновить сейчас» на карточке пресета (LLD-05 §3.3). */
sealed interface PresetRefresh {
    data class Updated(val version: Long) : PresetRefresh
    data class UpToDate(val version: Long) : PresetRefresh
    data class Failed(val message: String) : PresetRefresh
}

/** Сводка пресета из листинга хаба для пикера выбора (XR-119). */
data class HubPreset(val name: String, val version: Long, val rulesCount: Int)

/** Исход загрузки списка пресетов для пикера. */
sealed interface PresetList {
    data class Ok(val presets: List<HubPreset>) : PresetList
    data class Failed(val message: String) : PresetList
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
    /** Хвост единого журнала (XR-042): движок, пробы, смены сети/режима,
     *  файловые события. Живёт независимо от подключения, поллится из
     *  нативного журнала, пока приложение на переднем плане. */
    val logLines: List<String> = emptyList(),
    val debugExpanded: Boolean = false,
    /** SSID of the trusted network the tunnel is paused on, when [phase] is Paused. */
    val pausedSsid: String? = null,
    /** While paused: this trusted network failed the restriction probe (task 3b-2 §2). */
    val restrictedNetwork: Boolean = false,
    /** Дефолтной сети нет вообще: главная показывает «сети нет» вместо
     *  подписей про доверенную сеть и ограничения (XR-095). */
    val noNetwork: Boolean = false,
    /** SSID of the trusted network the tunnel is kept up on by explicit user
     *  choice ("Включить здесь"); null when no override is armed (XR-049). */
    val overrideSsid: String? = null,
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

    /** Политика при отказе всего пула серверов: true = block (fail-closed,
     *  проксируемый трафик режется, реальный IP не светится), false = direct
     *  (fail-open, проксируемое уходит напрямую). Дефолт block. */
    private val KEY_FAIL_CLOSED = "on_server_down_block"
    private val _failClosed = MutableStateFlow(prefs.getBoolean(KEY_FAIL_CLOSED, true))
    val failClosed: StateFlow<Boolean> = _failClosed

    fun setFailClosed(value: Boolean) {
        prefs.edit().putBoolean(KEY_FAIL_CLOSED, value).apply()
        _failClosed.value = value
    }

    private val _uiState = MutableStateFlow(VpnUiState())
    val uiState: StateFlow<VpnUiState> = _uiState

    private val _onboardingState = MutableStateFlow<OnboardingState>(OnboardingState.Loading())
    val onboardingState: StateFlow<OnboardingState> = _onboardingState

    private val _permissionRequest = MutableSharedFlow<Intent>(extraBufferCapacity = 1)
    val permissionRequest: SharedFlow<Intent> = _permissionRequest

    private val _messages = MutableSharedFlow<UiMessage>(extraBufferCapacity = 4)
    val messages: SharedFlow<UiMessage> = _messages

    // ── APK self-update (LLD-12) ────────────────────────────────────
    private val updateManager = UpdateManager(application)

    private val _updateState = MutableStateFlow<UpdateUiState>(UpdateUiState.Idle)
    val updateState: StateFlow<UpdateUiState> = _updateState

    // Крестик на уведомлении главной закрывает его для этой версии насовсем
    // (выбор владельца, XR-041): уведомление и пульс точки уходят, но сама
    // точка и предложение на «Серверах» остаются, пока обновление не
    // поставлено. Версия закрытого хранится в prefs и переживает перезапуск;
    // более новый релиз показывает уведомление снова.
    private val keyDeferredVersionCode = "update_deferred_code"
    private val _updateDeferred = MutableStateFlow(false)
    val updateDeferred: StateFlow<Boolean> = _updateDeferred
    private var deferredVersionCode = prefs.getLong(keyDeferredVersionCode, 0L)

    // Занятость РУЧНОЙ проверки: только спиннер кнопки «Проверить обновления».
    // Отдельный флаг вместо состояния Checking, чтобы уже известное
    // предложение не пропадало на время перепроверки (закреплённый баннер
    // сверху «Серверов» дёргал страницу, а точка мигала).
    private val _updateChecking = MutableStateFlow(false)
    val updateChecking: StateFlow<Boolean> = _updateChecking

    // Small de-dup window between *automatic* checks, NOT a throttle: the
    // triggers are already rare key events (app brought to foreground, fresh
    // connect), so we check on each one. This only coalesces a near-simultaneous
    // double-fire (e.g. foreground + auto-connect on open). Manual checks bypass
    // it. A deliberate re-open minutes later still checks — that was the bug with
    // the old multi-hour floor (it ate the very event the user cares about).
    private val autoUpdateCheckDedupMs = 60L * 1000
    // Метка последней внятной проверки живёт В ПАМЯТИ процесса, не в prefs:
    // дедуп защищает только от дублей триггеров внутри одной сессии, а само
    // Available-состояние перезапуск не переживает. Персистентная метка
    // глушила проверку свежего процесса, и рестарт в пределах 60с после
    // успешной проверки оставался без баннера и точки вовсе (XR-041).
    private var lastUpdateCheckDoneMs = 0L
    // Одна фоновая проверка за раз: она ретраит с бэкофом, и второй триггер,
    // прилетевший посреди ретраев, не должен запускать параллельный прогон
    // (XR-024). Job вместо флага, чтобы уход в фон гасил ретраи cancel'ом.
    private var autoUpdateJob: Job? = null
    // Плановая перепроверка, пока приложение на переднем плане: длинная сессия
    // иначе не узнает о релизе до следующего перезахода в приложение (XR-041).
    private val updateRecheckMs = 6L * 60 * 60 * 1000
    private var updateRecheckJob: Job? = null
    // Читается из binder-потока ConnectivityManager, отсюда volatile.
    @Volatile private var appForeground = false

    // Checks for updates on a real app foreground (background→foreground) — the
    // key "user opened the app" event, fired once per transition, NOT on rotation
    // or internal navigation (unlike Activity.onStart). Registering while the app
    // is already STARTED delivers onStart immediately, so the initial open is
    // covered too. Removed in onCleared.
    private val foregroundObserver = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            appForeground = true
            checkForUpdates(manual = false)
            startUpdateRecheck()
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
            startLogPolling()
        }

        override fun onStop(owner: LifecycleOwner) {
            appForeground = false
            stopLogPolling()
            stopUpdateRecheck()
            // Ретраи за спиной не нужны: баннер всё равно некому показывать, а
            // следующий выход на передний план запускает свежую проверку.
            autoUpdateJob?.cancel()
            autoUpdateJob = null
        }
    }

    // Появление дефолтной сети (в том числе смена её на поднятый VPN) это
    // повод повторить проверку обновлений: холодный старт часто стреляет
    // раньше связности, и без сети ретраи уходят в молоко (XR-041).
    private val connectivityManager =
        application.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    private val updateNetworkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            if (!appForeground) return
            // Колбэк приходит из binder-потока, вся логика проверки живёт на
            // main: перекидываем через viewModelScope.
            viewModelScope.launch { checkForUpdates(manual = false) }
        }
    }

    // ── Единый журнал (XR-042) ──────────────────────────────────────
    // Лента больше не едет через снапшот сервиса: журнал живёт своей жизнью
    // (движок может быть остановлен), поэтому VM поллит его хвост напрямую,
    // пока приложение на переднем плане.

    private var logPollJob: Job? = null

    private fun startLogPolling() {
        if (logPollJob != null) return
        logPollJob = viewModelScope.launch {
            while (true) {
                refreshLog()
                delay(1000)
            }
        }
    }

    private fun stopLogPolling() {
        logPollJob?.cancel()
        logPollJob = null
    }

    private suspend fun refreshLog() {
        val lines = withContext(Dispatchers.IO) {
            NativeBridge.nativeJournalTail().split('\n').filter { it.isNotEmpty() }
        }
        if (lines != _uiState.value.logLines) {
            _uiState.value = _uiState.value.copy(logLines = lines)
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
                    emitMessage(str(R.string.vpn_update_installed), UiSeverity.Info)
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
                    emitMessage(
                        str(R.string.vpn_update_install_failed, status.message),
                        UiSeverity.Error,
                    )
                    _updateState.value = UpdateUiState.Error("install: ${status.message}")
                }
            }
        }
        // Проверка обновлений событийная, на КЛЮЧЕВЫЕ события: выход
        // приложения на передний план (ProcessLifecycleOwner, реальный
        // background->foreground), свежий переход в Connected (applyServiceState)
        // и появление дефолтной сети (updateNetworkCallback). События редкие,
        // поэтому без большого пола, только 60с дедуп от двойного срабатывания.
        // addObserver при уже STARTED сразу дёргает onStart, так что первое
        // открытие тоже покрыто.
        ProcessLifecycleOwner.get().lifecycle.addObserver(foregroundObserver)
        runCatching { connectivityManager.registerDefaultNetworkCallback(updateNetworkCallback) }
    }

    private fun initialOnboardingState(): OnboardingState =
        if (repo.servers.value.isEmpty()) OnboardingState.ShowingWelcome
        else OnboardingState.Completed

    override fun onCleared() {
        ProcessLifecycleOwner.get().lifecycle.removeObserver(foregroundObserver)
        runCatching { connectivityManager.unregisterNetworkCallback(updateNetworkCallback) }
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
        when (serverSelectAction(_uiState.value.phase, repo.activeId.value, id)) {
            SwitchAction.Ignore -> return
            SwitchAction.SetActive -> repo.setActive(id)
            SwitchAction.Reconnect -> {
                repo.setActive(id)
                reconnectActive(str(R.string.vpn_switching_server))
            }
        }
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
            reconnectActive(str(R.string.vpn_applying_settings))
        }
    }

    /** Живой реконнект, ровно один. Ожидание `Idle` внутри него ничем не
     *  отличает свой disconnect от чужого, поэтому нажатое посреди
     *  переключения «Отключить» подняло бы туннель обратно, а второе
     *  переключение подряд наложило бы два цикла друг на друга. */
    private var reconnectJob: Job? = null

    /** Погасить туннель и поднять его заново на текущем активном профиле.
     *  Один путь на правку активного сервера и на переключение между
     *  серверами (XR-088): конфигурацию движок читает на старте, поэтому
     *  подхватить новый профиль он может только через полный цикл. Ход
     *  виден по фазам подключения на главном экране, [notice] говорит,
     *  почему туннель мигнул. */
    private fun reconnectActive(notice: String) {
        emitMessage(notice, UiSeverity.Info)
        reconnectJob?.cancel()
        reconnectJob = viewModelScope.launch {
            disconnect(cancelReconnect = false)
            _uiState.map { it.phase == ConnectPhase.Idle }.first { it }
            delay(300)
            onConnectClicked()
        }
    }

    fun clearLog() {
        NativeBridge.nativeJournalClear()
        viewModelScope.launch { refreshLog() }
    }

    // ── Log tab (LLD-03) ────────────────────────────────────────────

    fun updateLogQuery(q: String) {
        _uiState.value = _uiState.value.copy(logQuery = q)
    }

    fun toggleLogRegexMode() {
        _uiState.value = _uiState.value.copy(logRegexMode = !_uiState.value.logRegexMode)
    }

    /** Full, unfiltered log — toolbar actions always operate on this, the
     *  search field is only a visual filter (LLD-03 §2.4). Читается с диска
     *  целиком (весь журнал, не только хвост вкладки), поэтому off-main. */
    private suspend fun buildFullLog(): String =
        withContext(Dispatchers.IO) { NativeBridge.nativeJournalDump() }

    fun copyLog() {
        viewModelScope.launch {
            val text = buildFullLog()
            val cm = getApplication<Application>()
                .getSystemService(Context.CLIPBOARD_SERVICE) as? android.content.ClipboardManager
            if (cm == null) {
                emitMessage(str(R.string.logs_clipboard_unavailable), UiSeverity.Warn)
                return@launch
            }
            cm.setPrimaryClip(android.content.ClipData.newPlainText("xr-proxy log", text))
            emitMessage(str(R.string.main_copied), UiSeverity.Info)
        }
    }

    fun shareLog(context: Context) {
        viewModelScope.launch {
            try {
                val text = buildFullLog()
                val file = withContext(Dispatchers.IO) {
                    File(context.cacheDir, "xr-proxy.log").also { it.writeText(text) }
                }
                val uri = androidx.core.content.FileProvider.getUriForFile(
                    context, "${context.packageName}.fileprovider", file,
                )
                val intent = Intent(Intent.ACTION_SEND).apply {
                    type = "text/plain"
                    putExtra(Intent.EXTRA_STREAM, uri)
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                }
                context.startActivity(
                    Intent.createChooser(intent, str(R.string.logs_share_chooser)),
                )
            } catch (e: Exception) {
                emitMessage(str(R.string.logs_share_failed, e.message.orEmpty()), UiSeverity.Error)
            }
        }
    }

    /** Write the full log to a user-chosen SAF document (LLD-03 §3.5). */
    fun writeLogTo(uri: android.net.Uri, resolver: android.content.ContentResolver) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val text = NativeBridge.nativeJournalDump()
                resolver.openOutputStream(uri)?.use { out ->
                    out.writer(Charsets.UTF_8).use { w -> w.write(text) }
                }
                emitMessage(str(R.string.logs_saved), UiSeverity.Info)
            } catch (e: Exception) {
                emitMessage(str(R.string.logs_save_failed, e.message.orEmpty()), UiSeverity.Error)
            }
        }
    }

    // ── Ротация журнала (XR-042) ────────────────────────────────────

    private val _journalMaxKb = MutableStateFlow(JournalSettings.maxKb(prefs))
    val journalMaxKb: StateFlow<Int> = _journalMaxKb

    private val _journalMaxFiles = MutableStateFlow(JournalSettings.maxFiles(prefs))
    val journalMaxFiles: StateFlow<Int> = _journalMaxFiles

    fun setJournalRotation(maxKb: Int, maxFiles: Int) {
        JournalSettings.setRotation(
            prefs, getApplication<Application>().filesDir, maxKb, maxFiles,
        )
        _journalMaxKb.value = maxKb
        _journalMaxFiles.value = maxFiles
    }

    // ── Trusted networks / auto-pause (task 3b-2) ───────────────────

    fun addTrustedNetwork(ssid: String) {
        val clean = ssid.trim()
        if (clean.isBlank()) {
            emitMessage(str(R.string.vpn_trusted_enter_ssid), UiSeverity.Info)
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

    /** Keep the tunnel running on the current trusted network ("Включить здесь").
     *  Falls back to the intent path when the binder is not (yet) connected, so
     *  the tap is never silently lost (XR-049). */
    fun resumeOnTrustedNetwork() {
        val svc = boundService
        if (svc != null) {
            svc.resumeOnTrustedNetwork()
            return
        }
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_RESUME_OVERRIDE
        }
        try { getApplication<Application>().startService(intent) } catch (_: Exception) {}
        tryBind(autoCreate = false)
    }

    /** Put the tunnel back on auto-pause on the current trusted network,
     *  dropping the "Включить здесь" override (XR-049). Same intent fallback
     *  as [resumeOnTrustedNetwork]. */
    fun pauseOnTrustedNetwork() {
        val svc = boundService
        if (svc != null) {
            svc.pauseOnTrustedNetwork()
            return
        }
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_PAUSE_OVERRIDE
        }
        try { getApplication<Application>().startService(intent) } catch (_: Exception) {}
        tryBind(autoCreate = false)
    }

    // ── Правила маршрутизации (LLD-05, XR-047) ──────────────────────

    private val _userRules = MutableStateFlow(
        UserRulesStore.load(application.filesDir)
    )
    /** Глобальный упорядоченный список «моих правил»: поверх пресета любого
     *  активного сервера, первое совпадение выигрывает. */
    val userRules: StateFlow<List<UserRule>> = _userRules

    /** Сохранить новый список целиком: состояние сразу, диск и живой движок
     *  в фоне. Правка действует на живом туннеле без переподключения
     *  (XR-180): движок пересобирает merged-роутер тем же путём, каким
     *  подхватывает новую версию пресета. Незапущенный движок правила
     *  заберёт ближайшим Connect через [buildConfigJson]. */
    fun saveUserRules(rules: List<UserRule>) {
        val capped = rules.take(UserRulesStore.MAX_RULES)
        _userRules.value = capped
        viewModelScope.launch(Dispatchers.IO) {
            try {
                UserRulesStore.save(getApplication<Application>().filesDir, capped)
            } catch (e: Exception) {
                emitMessage(
                    str(R.string.vpn_rules_save_failed, e.message.orEmpty()),
                    UiSeverity.Error,
                )
            }
            try {
                // Пустое действие по умолчанию значит «как в конфиге старта»:
                // само значение живёт в ядре (XR-271).
                NativeBridge.nativeApplyUserRules(
                    UserRulesStore.toConfigJson(capped).toString(),
                    "",
                )
            } catch (e: Throwable) {
                Log.w("xr-rules", "не удалось применить правила на лету: $e")
            }
        }
    }

    /** Кэшированный пресет активного сервера для карточки и просмотра. */
    fun readCachedPreset(presetName: String): CachedPreset? =
        PresetCacheReader.read(presetCacheDir, presetName)

    /** Превью блока `[routing]` для кнопки `{ }` на экране правил: мои правила
     *  поверх пресета хаба. Собирает ядро рядом с кэшем пресета (XR-271),
     *  поэтому вызов ходит на диск и живёт на IO. */
    suspend fun mergedToml(rules: List<UserRule>): String = withContext(Dispatchers.IO) {
        NativeBridge.nativeMergedToml(
            presetCacheDir.absolutePath,
            repo.activeServer()?.hubPreset.orEmpty(),
            UserRulesStore.toConfigJson(rules).toString(),
            "",
        )
    }

    /** Форсированный fetch пресета с хаба («Обновить сейчас»). */
    suspend fun refreshPresetNow(): PresetRefresh {
        val server = repo.activeServer()
        val hubUrl = server?.hubUrl?.takeIf { it.isNotBlank() }
        val preset = server?.hubPreset?.takeIf { it.isNotBlank() }
        if (hubUrl == null || preset == null) {
            return PresetRefresh.Failed(str(R.string.vpn_preset_no_hub))
        }
        val json = withContext(Dispatchers.IO) {
            NativeBridge.nativeRefreshPreset(hubUrl, preset, presetCacheDir.absolutePath, 5_000L)
        }
        val obj = runCatching { JSONObject(json) }.getOrNull()
            ?: return PresetRefresh.Failed(str(R.string.vpn_hub_bad_response))
        obj.optString("error").takeIf { it.isNotBlank() }?.let { code ->
            return PresetRefresh.Failed(friendlyPresetError(code, preset))
        }
        val version = obj.optLong("version", 0)
        return if (obj.optBoolean("updated")) PresetRefresh.Updated(version)
        else PresetRefresh.UpToDate(version)
    }

    private fun friendlyPresetError(code: String, preset: String): String = when {
        code == "not_found" -> str(R.string.vpn_preset_gone, preset)
        code.startsWith("network") -> str(R.string.vpn_hub_unreachable)
        code.startsWith("http_") -> str(R.string.vpn_hub_error, code.removePrefix("http_"))
        else -> str(R.string.vpn_preset_refresh_failed, code)
    }

    /** Список пресетов активного хаба для пикера выбора (XR-119). */
    suspend fun listHubPresets(): PresetList {
        val hubUrl = activeHubUrl() ?: return PresetList.Failed(str(R.string.vpn_hub_not_set))
        val json = withContext(Dispatchers.IO) {
            NativeBridge.nativeListPresets(hubUrl, 5_000L)
        }
        val obj = runCatching { JSONObject(json) }.getOrNull()
            ?: return PresetList.Failed(str(R.string.vpn_hub_bad_response))
        obj.optString("error").takeIf { it.isNotBlank() }?.let { code ->
            return PresetList.Failed(friendlyHubError(code))
        }
        val arr = obj.optJSONArray("presets")
            ?: return PresetList.Failed(str(R.string.vpn_hub_bad_response))
        val out = buildList {
            for (i in 0 until arr.length()) {
                val o = arr.optJSONObject(i) ?: continue
                add(HubPreset(o.optString("name"), o.optLong("version"), o.optInt("rules_count")))
            }
        }
        return PresetList.Ok(out)
    }

    /** Записать выбранный пресет в активный профиль. Применится на следующем
     *  подключении: buildConfigJson читает hubPreset при старте. */
    fun setActivePreset(name: String) {
        val server = repo.activeServer() ?: return
        if (server.hubPreset == name) return
        repo.upsert(server.copy(hubPreset = name))
    }

    private fun friendlyHubError(code: String): String = when {
        code.startsWith("network") -> str(R.string.vpn_hub_unreachable)
        code.startsWith("http_") -> str(R.string.vpn_hub_error, code.removePrefix("http_"))
        else -> str(R.string.vpn_preset_list_failed, code)
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
            if (manual) emitMessage(str(R.string.vpn_update_needs_hub), UiSeverity.Info)
            return
        }
        // Never interrupt an in-flight download / install. Припаркованное
        // ReadyToInstall не в счёт: перепроверка может найти релиз ещё новее.
        when (_updateState.value) {
            is UpdateUiState.Downloading, is UpdateUiState.Installing -> return
            else -> {}
        }
        // A background check already retrying covers this trigger.
        if (!manual && autoUpdateJob?.isActive == true) return
        // В фоне не проверяем: показывать баннер некому, а бессрочные ретраи
        // жгли бы сеть за спиной. Выход на передний план сам запустит проверку.
        if (!manual && !appForeground) return
        // Rate-limit only CONCLUSIVE background checks (60s) so a burst of events
        // doesn't spam the hub. A FAILED attempt must not count: the cold-start
        // trigger often fires before connectivity is up, and stamping on a failure
        // used to poison the window so the banner never appeared until much later
        // (XR-024). So we stamp on Available/UpToDate only, inside the loop.
        val now = System.currentTimeMillis()
        if (!manual && now - lastUpdateCheckDoneMs < autoUpdateCheckDedupMs) return
        if (manual) {
            // Ручная проверка главнее фоновой: гасим её ретраи, чтобы поздний
            // фоновый результат не переписал показанный пользователю ответ.
            autoUpdateJob?.cancel()
            // Спиннер только у явной проверки; состояние предложения не
            // трогаем, баннер и точка не мигают на время перепроверки.
            _updateChecking.value = true
        }
        val job = viewModelScope.launch {
            // Фоновая проверка ретраит с бэкофом до потолка, пока не получит
            // внятный ответ. Раньше четыре попытки укладывались в ~46 секунд, и
            // хаб, недоступный чуть дольше (сеть поднялась поздно, VPS мигнул),
            // оставлял пользователя без баннера до следующего события (XR-041).
            // Уход приложения в фон гасит цикл (onStop); ручная проверка идёт
            // одной попыткой и отвечает ошибкой сразу.
            val backoffMs = longArrayOf(0L, 4_000L, 12_000L, 30_000L, 60_000L, 180_000L, 300_000L)
            var attempt = 0
            while (true) {
                if (attempt > 0) delay(backoffMs[minOf(attempt, backoffMs.lastIndex)])
                val result = withContext(Dispatchers.IO) { updateManager.check(hubUrl) }
                when (result) {
                    is UpdateManager.CheckResult.Available -> {
                        lastUpdateCheckDoneMs = System.currentTimeMillis()
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
                        // Отсрочка «Позже» держится на том же релизе; более
                        // новый снова показывает баннер и пульс.
                        _updateDeferred.value =
                            result.release.versionCode <= deferredVersionCode
                        return@launch
                    }
                    is UpdateManager.CheckResult.UpToDate -> {
                        lastUpdateCheckDoneMs = System.currentTimeMillis()
                        _updateState.value =
                            if (manual) UpdateUiState.UpToDate else UpdateUiState.Idle
                        _updateDeferred.value = false
                        return@launch
                    }
                    is UpdateManager.CheckResult.Failed -> {
                        // Manual: surface the error now. Background: keep retrying
                        // and do NOT stamp the rate-limit, so the next trigger is
                        // free to try too.
                        if (manual) {
                            // Известное предложение ошибкой перепроверки не
                            // затираем (баннер и точка остаются), ошибка уходит
                            // в снекбар; без предложения она встаёт в секцию
                            // обновления на «Серверах».
                            when (_updateState.value) {
                                is UpdateUiState.Available, is UpdateUiState.ReadyToInstall ->
                                    emitMessage(friendlyUpdateError(result.error), UiSeverity.Error)
                                else ->
                                    _updateState.value =
                                        UpdateUiState.Error(friendlyUpdateError(result.error))
                            }
                            return@launch
                        }
                    }
                }
                attempt++
            }
        }
        // Спиннер кнопки гаснет и при отмене (уход в фон, onCleared).
        if (manual) job.invokeOnCompletion { _updateChecking.value = false }
        if (!manual) autoUpdateJob = job
    }

    private fun startUpdateRecheck() {
        if (updateRecheckJob != null) return
        updateRecheckJob = viewModelScope.launch {
            while (true) {
                delay(updateRecheckMs)
                checkForUpdates(manual = false)
            }
        }
    }

    private fun stopUpdateRecheck() {
        updateRecheckJob?.cancel()
        updateRecheckJob = null
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
            emitMessage(str(R.string.vpn_update_allow_source), UiSeverity.Info)
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
        val release = when (val s = _updateState.value) {
            is UpdateUiState.Available -> s.release
            is UpdateUiState.ReadyToInstall -> s.release
            else -> return
        }
        deferredVersionCode = release.versionCode
        prefs.edit().putLong(keyDeferredVersionCode, release.versionCode).apply()
        _updateDeferred.value = true
    }

    private fun friendlyUpdateError(code: String): String = when {
        code == "no_release" -> str(R.string.vpn_update_no_release)
        code == "no_hub" -> str(R.string.vpn_update_no_hub)
        code == "no_release_key" -> str(R.string.vpn_update_no_key)
        code == "sha_mismatch" -> str(R.string.vpn_update_sha_mismatch)
        code.startsWith("verify") -> str(R.string.vpn_update_bad_signature)
        code.startsWith("network") || code.startsWith("http") ->
            str(R.string.vpn_hub_unreachable)
        else -> str(R.string.vpn_update_failed, code)
    }

    fun toggleDebug() {
        _uiState.value = _uiState.value.copy(debugExpanded = !_uiState.value.debugExpanded)
    }

    // ── Onboarding (LLD-04 + LLD-08) ───────────────────────────────

    fun onInviteLinkReceived(raw: String) {
        _onboardingState.value = OnboardingState.Loading()
        viewModelScope.launch {
            val parsedJson = withContext(Dispatchers.IO) {
                NativeBridge.nativeParseInviteLink(raw)
            }
            val parsed = runCatching { JSONObject(parsedJson) }.getOrNull()
            if (parsed == null || parsed.has("error")) {
                val err = parsed?.optString("error") ?: "parse failed"
                Log.w("xr-onboarding", "parseInviteLink: $err")
                _onboardingState.value = badInviteError(hubUrl = "", raw = raw,
                    detail = str(R.string.vpn_invite_bad_format))
                return@launch
            }
            val hubUrl = parsed.optString("hub_url")
            val token = parsed.optString("token")
            if (hubUrl.isBlank() || token.isBlank()) {
                _onboardingState.value = badInviteError(hubUrl = hubUrl, raw = raw,
                    detail = str(R.string.vpn_invite_bad_format))
                return@launch
            }

            // Адрес хаба известен ещё до сети: разбор ссылки локальный.
            // Дальше экран ожидания говорит, к кому именно идёт запрос.
            _onboardingState.value = OnboardingState.Loading(hubUrl)
            val infoJson = withContext(Dispatchers.IO) {
                NativeBridge.nativeFetchInviteInfo(
                    hubUrl, token, presetCacheDir.absolutePath, 5_000L,
                )
            }
            val info = runCatching { JSONObject(infoJson) }.getOrNull()
            if (info == null) {
                _onboardingState.value =
                    hubInviteError(hubUrl, raw, str(R.string.vpn_hub_bad_response))
                return@launch
            }
            if (info.has("error")) {
                _onboardingState.value = inviteErrorFor(info.optString("error"), hubUrl, raw)
                return@launch
            }

            _onboardingState.value = OnboardingState.ConfirmInvite(
                hubUrl = hubUrl,
                token = token,
                preset = info.optString("preset"),
                comment = info.optString("comment"),
                status = info.optString("status", "active"),
                expiresAt = info.optString("expires_at"),
                reclaimable = info.optBoolean("reclaimable"),
            )
        }
    }

    fun onInviteCancelled() {
        _onboardingState.value = initialOnboardingState()
    }

    /** «Повторить» с экрана отказа: тот же путь, что и первый заход, ссылка
     *  сохранена в состоянии, вставлять её заново не нужно. */
    fun onInviteRetry() {
        val err = _onboardingState.value as? OnboardingState.InviteError ?: return
        onInviteLinkReceived(err.rawLink)
    }

    /** Дело в самом приглашении: повторять нечего, только назад. */
    private fun badInviteError(hubUrl: String, raw: String, detail: String) =
        OnboardingState.InviteError(
            hubUrl = hubUrl,
            title = str(R.string.vpn_invite_bad_title),
            detail = detail,
            retryable = false,
            rawLink = raw,
        )

    /** Дело в связи или хабе: повтор осмыслен. */
    private fun hubInviteError(hubUrl: String, raw: String, detail: String) =
        OnboardingState.InviteError(
            hubUrl = hubUrl,
            title = str(R.string.vpn_invite_hub_silent),
            detail = detail,
            retryable = true,
            rawLink = raw,
        )

    /** Код ошибки хаба раскладывается на два класса: not_found и gone это
     *  судьба самого приглашения, остальное это проблемы доставки. */
    private fun inviteErrorFor(code: String, hubUrl: String, raw: String) = when (code) {
        "not_found", "gone" -> badInviteError(hubUrl, raw, friendlyInviteInfoError(code))
        else -> hubInviteError(hubUrl, raw, friendlyInviteInfoError(code))
    }

    fun onManualSetupChosen() {
        _onboardingState.value = OnboardingState.Completed
    }

    fun onInviteConfirmed() {
        val current = _onboardingState.value as? OnboardingState.ConfirmInvite ?: return
        if (current.applyInProgress) return
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

            // Раскладку payload'а на профиль (пул по приоритету, легаси-адрес
            // запасным вариантом, дефолты обфускации) делает ядро, XR-271.
            val fields = result.optJSONObject("profile") ?: run {
                emitMessage(friendlyClaimError("unknown"), UiSeverity.Error)
                _onboardingState.value = current.copy(applyInProgress = false)
                return@launch
            }
            val serverAddr = fields.optString("server_address")
            val hubFromPayload = fields.optString("hub_url").ifBlank { current.hubUrl }

            val profile = ServerProfile(
                id = UUID.randomUUID().toString(),
                name = repo.generateName(getApplication(), serverAddr, hubFromPayload, current.comment),
                serverAddress = serverAddr,
                serverPort = fields.optInt("server_port", 8443),
                endpoints = parseProfileEndpoints(fields),
                obfuscationKey = fields.optString("obfuscation_key"),
                modifier = fields.optString("modifier"),
                salt = fields.optLong("salt"),
                hubUrl = hubFromPayload,
                hubPreset = fields.optString("preset"),
                trustedPublicKey = publicKey,
                inviteToken = current.token,
                createdAt = OffsetDateTime.now().toString(),
                source = ServerSource.Invite,
            )
            repo.upsert(profile)
            // Под живым туннелем инвайт только пополняет список серверов
            // (XR-088): активный профиль читается движком на старте, и
            // подмена его на ходу развела бы карточку с реальным трафиком.
            if (inviteActivatesProfile(_uiState.value.phase, repo.activeId.value != null)) {
                repo.setActive(profile.id)
            } else {
                emitMessage(str(R.string.vpn_profile_added), UiSeverity.Info)
            }

            if (!presetCached) {
                emitMessage(str(R.string.vpn_preset_unsigned), UiSeverity.Warn)
            }
            _onboardingState.value = OnboardingState.Completed
        }
    }

    /** Пул профиля из ответа ядра: порядок в массиве уже приоритетный. */
    private fun parseProfileEndpoints(fields: JSONObject): List<ProfileEndpoint> {
        val arr = fields.optJSONArray("endpoints") ?: return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val o = arr.optJSONObject(i) ?: return@mapNotNull null
            ProfileEndpoint(
                name = o.optString("name", ""),
                address = o.optString("address"),
                port = o.optInt("port", 8443),
            )
        }
    }

    private fun friendlyInviteInfoError(code: String): String = when (code) {
        "not_found" -> str(R.string.vpn_invite_not_found)
        "gone" -> str(R.string.vpn_invite_gone)
        else -> when {
            code.startsWith("network") -> str(R.string.vpn_invite_check_network)
            code.contains("certificate") -> str(R.string.vpn_invite_insecure)
            code.startsWith("http_") -> str(R.string.vpn_hub_error, code.removePrefix("http_"))
            else -> str(R.string.vpn_error_code, code)
        }
    }

    private fun friendlyClaimError(code: String): String = when {
        code.contains("gone") -> str(R.string.vpn_invite_gone)
        code.contains("not_found") -> str(R.string.vpn_invite_not_found)
        code.contains("network") -> str(R.string.vpn_hub_unreachable)
        code.contains("certificate") -> str(R.string.vpn_invite_insecure)
        else -> str(R.string.vpn_invite_apply_failed, code)
    }

    /** Текст из ресурсов приложения. Строки VM уезжают в снекбары и в
     *  состояние экранов, локаль берётся системная (XR-092). */
    private fun str(id: Int, vararg args: Any): String =
        getApplication<Application>().getString(id, *args)

    private fun emitMessage(text: String, severity: UiSeverity) {
        viewModelScope.launch { _messages.emit(UiMessage(text, severity)) }
    }

    // ── VPN connection ──────────────────────────────────────────────

    fun onConnectClicked() {
        val s = _uiState.value
        if (s.phase != ConnectPhase.Idle) return
        val server = repo.activeServer()
        if (server == null || server.effectiveEndpoints.isEmpty() || server.obfuscationKey.isBlank()) {
            emitMessage(str(R.string.vpn_fill_server_and_key), UiSeverity.Info)
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
            emitMessage(str(R.string.vpn_permission_denied), UiSeverity.Info)
        }
    }

    private fun actuallyStart() {
        val server = repo.activeServer() ?: return
        val configJson = buildConfigJson(server) ?: run {
            _uiState.value = _uiState.value.copy(
                phase = ConnectPhase.Idle, state = "Disconnected",
            )
            return
        }
        val intent = Intent(getApplication(), XrVpnService::class.java).apply {
            action = XrVpnService.ACTION_START
            putExtra(XrVpnService.EXTRA_CONFIG_JSON, configJson)
        }
        getApplication<Application>().startForegroundService(intent)
        tryBind(autoCreate = true)
        _uiState.value = _uiState.value.copy(phase = ConnectPhase.Preparing, state = "Connecting...")
    }

    /** [cancelReconnect] снимает живой авто-реконнект: отключение руками
     *  главнее переключения сервера, иначе ожидание `Idle` внутри
     *  [reconnectActive] примет ручной disconnect за свой и поднимет туннель
     *  обратно. Сам реконнект гасит туннель этим же вызовом и свою отмену не
     *  заказывает. */
    fun disconnect(cancelReconnect: Boolean = true) {
        if (cancelReconnect) {
            reconnectJob?.cancel()
            reconnectJob = null
        }
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
            pausedSsid = svcState.pausedSsid,
            restrictedNetwork = svcState.restrictedNetwork,
            noNetwork = svcState.noNetwork,
            overrideSsid = svcState.overrideSsid,
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

    /** Конфиг движка собирает ядро (XR-271): порядок пула, дефолты обфускации,
     *  чистка резолверов системы и экранирование живут там же, где движок этот
     *  конфиг читает. Приложение отдаёт профиль как есть и получает готовый
     *  JSON, либо null, если ядро отказалось (тогда причина уже в снекбаре). */
    private fun buildConfigJson(server: ServerProfile): String? {
        val endpoints = JSONArray()
        server.effectiveEndpoints.forEach {
            endpoints.put(
                JSONObject()
                    .put("name", it.name)
                    .put("address", it.address)
                    .put("port", it.port),
            )
        }
        val dns = JSONArray()
        collectSystemDnsServers().forEach { dns.put(it) }
        val profile = JSONObject()
            // Legacy-поля с primary остаются рядом с пулом: движок и старые
            // версии читают их как раньше (LLD-10).
            .put("server_address", server.serverAddress)
            .put("server_port", server.serverPort)
            .put("servers", endpoints)
            .put("obfuscation_key", server.obfuscationKey)
            .put("modifier", server.modifier)
            .put("salt", server.salt)
            .put("hub_url", server.hubUrl)
            .put("hub_preset", server.hubPreset)
            .put("hub_cache_dir", presetCacheDir.absolutePath)
            // Мои правила уезжают массивом user_rules (LLD-05): движок соберёт
            // merged-роутер сам, докачав пресет хаба из hub-полей выше.
            .put("user_rules", UserRulesStore.toConfigJson(_userRules.value))
            .put("dns_resolvers", dns)
            .put("fail_closed", _failClosed.value)

        val json = NativeBridge.nativeBuildConfig(profile.toString())
        val error = runCatching { JSONObject(json).optString("error") }.getOrNull()
        if (error.isNullOrBlank() || error == "null") return json
        emitMessage(str(R.string.vpn_fill_server_and_key), UiSeverity.Info)
        Log.w("xr-config", "ядро отвергло профиль: $error")
        return null
    }

    /** Резолверы дефолтной сети как их отдала система. Что из них годится
     *  движку, решает ядро при сборке конфига. */
    private fun collectSystemDnsServers(): List<String> {
        val cm = getApplication<Application>()
            .getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            ?: return emptyList()
        val network = cm.activeNetwork ?: return emptyList()
        val lp: LinkProperties = cm.getLinkProperties(network) ?: return emptyList()
        return lp.dnsServers.mapNotNull { it.hostAddress }
    }

}
