package com.xrproxy.app.ui.files

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.R
import com.xrproxy.app.data.ShareRepository
import com.xrproxy.app.data.ShareStore
import com.xrproxy.app.data.StorageAccess
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.FileGrouping
import com.xrproxy.app.model.FileSort
import com.xrproxy.app.model.GitLogEntry
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareGrant
import com.xrproxy.app.model.SortOrder
import com.xrproxy.app.model.isSelected
import com.xrproxy.app.service.ShareSyncScheduler
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File

/**
 * Drives the Files screen (LLD-19, XR-031): a list of shares ("drives") and an
 * Explorer over one share's folders. Files mirror into the app's own directory.
 *
 * One axis of state per file (XR-044): "on the device or on its way". The
 * per-row control queues a download, cancels it, retries after a failure or
 * deletes the local copy; the selection set is kept in lockstep with those
 * actions, so the background mirror wants exactly what the rows show. Opening
 * a share re-queues wanted-but-absent files, which also restores a queue lost
 * to process death.
 */
class FilesViewModel(app: Application) : AndroidViewModel(app) {

    // Opening the store means EncryptedSharedPreferences: a Keystore IPC plus
    // a file decrypt, up to seconds on a cold process. Doing that in the
    // constructor froze the first frame of the Files tab (XR-093), so it is
    // built on IO and everything reaches it through store().
    private val storeDeferred = viewModelScope.async(Dispatchers.IO) { ShareStore.create(app) }
    private val repo = ShareRepository(app)

    private suspend fun store(): ShareStore = storeDeferred.await()

    private val _configs = MutableStateFlow<List<ShareConfig>>(emptyList())
    val configs: StateFlow<List<ShareConfig>> = _configs

    /** Live progress of the running native transfer. [share] is the owner
     *  share id, empty for a storage migration; rows attribute progress (and a
     *  cancel) only to their own share's paths. */
    data class Progress(
        val share: String,
        val file: String,
        val filesDone: Long,
        val filesTotal: Long,
        val bytesDone: Long,
        val bytesTotal: Long,
        val speedBytesPerSec: Long,
    )

    /** One file waiting in (or at the head of) the download queue (XR-044).
     *  Deliberately not a data class: the queue tracks instances, and identity
     *  equality keeps a cancelled item distinct from the same file re-queued a
     *  moment later. */
    class QueueItem(val shareId: String, val entry: ManifestEntry) {
        fun matches(shareId: String, path: String): Boolean =
            this.shareId == shareId && entry.path == path
    }

    /** A download that broke mid-way: the row keeps the saved progress under a
     *  red tint and offers a retry that resumes from the partial (XR-044). */
    data class FailedDownload(
        val shareId: String,
        val path: String,
        val bytesDone: Long,
        val bytesTotal: Long,
        val error: String,
    )

    data class UiState(
        val hubShares: List<ShareGrant> = emptyList(),
        val loadingHub: Boolean = false,
        /** True once the share store has loaded: before that an empty [configs]
         *  means "still opening", not "no shares", and the empty-state text
         *  must not flash. */
        val storeReady: Boolean = false,
        /** True when the last invite refresh failed on the network layer
         *  (airplane mode, hub unreachable): the saved shares stay usable and
         *  the list is marked stale instead of toasting an error (XR-093). */
        val hubOffline: Boolean = false,
        val openShareId: String? = null,
        val currentPath: String = "",
        val manifest: List<ManifestEntry> = emptyList(),
        val manifestLoading: Boolean = false,
        /** True when the list shows the local files, not the agent's manifest:
         *  right after opening a share (cache-first, until the fresh manifest
         *  lands) and after an offline fetch failure. Combined with an empty
         *  [manifest] it renders the "no network, nothing downloaded" state. */
        val offlineLocal: Boolean = false,
        /** True when the offline listing is the persisted server manifest (full
         *  file list, XR-099), not just the downloaded files: the banner then
         *  says the listing is the last known one, and non-downloaded files stay
         *  visible. False means only local files are shown. */
        val offlineFullListing: Boolean = false,
        val localPaths: Set<String> = emptySet(),
        /** Порядок строк проводника (XR-251): общий на все шары и переживает
         *  перезапуск, поэтому едет из стора, а не живёт состоянием экрана. */
        val sortOrder: SortOrder = SortOrder(),
        /** Пути открытой шары, которые уже открывали с этого устройства
         *  (XR-251). Наличие файла на диске тут ни при чём: минус в строке
         *  снимает копию, а просмотр остаётся. */
        val viewedPaths: Set<String> = emptySet(),
        /** Фильтр «только непросмотренные» (XR-256): общий на все шары и живёт
         *  между запусками, как и порядок строк, поэтому едет из стора. */
        val unviewedOnly: Boolean = false,
        /** Режим группировки списка (XR-258): соседняя привычка проводника,
         *  живёт там же, где порядок строк и фильтр. */
        val grouping: FileGrouping = FileGrouping.NONE,
        /** FIFO of files queued with the per-row plus (XR-044); the head is the
         *  one being downloaded (or waiting out the background mirror's lock). */
        val queue: List<QueueItem> = emptyList(),
        /** Сколько файлов этого батча очередь уже прошла (XR-056): общий
         *  индикатор берёт из него размер батча, а сам счётчик обнуляется, как
         *  только новый батч встаёт в пустую очередь. */
        val queueDone: Int = 0,
        val failed: List<FailedDownload> = emptyList(),
        /** Snapshot of the native transfer, polled while the explorer is open;
         *  rows recognise themselves by share id + path, which also makes the
         *  background mirror's progress visible per row (XR-044). */
        val transfer: Progress? = null,
        /** Share whose storage migration is running (XR-043), or null. The
         *  migration card renders [transfer] while this is set. */
        val migratingShareId: String? = null,
        val openFileEvent: File? = null,
        val message: String? = null,
        /** Путь файла, чей экран информации открыт (XR-257), или null. Держим
         *  путь, а не саму строку: экран берёт её из свежего манифеста, поэтому
         *  переживает обновление списка, а пропавший из шары файл его закрывает. */
        val detailsPath: String? = null,
        /** Путь файла, чей экран истории коммитов открыт (LLD-33), или null.
         *  История видна только с правом записи: git-роуты агента стоят за
         *  share:write, и точечный отказ читателю чище, чем прятать кнопку. */
        val historyPath: String? = null,
        val history: List<GitLogEntry> = emptyList(),
        val historyLoading: Boolean = false,
        val historyError: String? = null,
        /** Путь файла, чей диалог правки открыт (LLD-33), или null. [editSha]
         *  это хеш строки манифеста на момент открытия: он уезжает в If-Match,
         *  и агент, чей файл менялся, отвечает 412, а не молча перетирает. */
        val editPath: String? = null,
        val editSha: String = "",
        val editText: String = "",
        val editLoading: Boolean = false,
        val editBusy: Boolean = false,
        /** Share whose storage-directory dialog is open (XR-043), or null. */
        val storageDialogFor: String? = null,
        /** True when the dialog is the first-sync prompt (auto-continues the
         *  deferred action on choice) vs. a later "change folder" from settings. */
        val storagePromptMode: Boolean = false,
        /** Share whose "Импорт по URL" dialog is open (LLD-29), or null. */
        val importDialogFor: String? = null,
        /** Импорты, за которыми следит приложение (XR-175): очередь держит сам
         *  агент, здесь строка на каждую свою джобу. Один цикл опрашивает их
         *  раз в 2 секунды; уход с экрана скачивание не прерывает. */
        val importJobs: List<ImportJob> = emptyList(),
        /** Причина сорвавшегося импорта, как её назвал агент (XR-161). Тост её
         *  обрезает: с Android 12 в нём две строки, а сюда приходит хвост
         *  stderr плагина в несколько абзацев. Поэтому отдельный диалог. */
        val importError: String? = null,
    )

    /** A URL-import job the open screen is tracking (LLD-29). */
    data class ImportJob(
        val shareId: String,
        val jobId: String,
        /** Percent from the agent; null until the plugin reports any. */
        val progress: Double? = null,
        /** Джоба ждёт своей очереди у агента (XR-175): прогресса ещё нет и не
         *  будет, пока воркер не дойдёт до неё, поэтому строка подписывается
         *  «в очереди», а не висит пустым процентом. */
        val queued: Boolean = true,
    )

    /** An action deferred until the user makes the first-sync storage choice. */
    private sealed interface Pending {
        data class Enqueue(val shareId: String, val entry: ManifestEntry) : Pending
        data class EnqueueFolder(val shareId: String, val path: String) : Pending
        data class EnableSync(val shareId: String) : Pending
    }

    private var pending: Pending? = null

    // The invite the Files screen was opened with, kept so a share opened with a
    // stale token can pull a fresh grant without threading these through every
    // call (XR-167). Set on each refreshHub, which the screen calls on entry.
    private var hubUrl: String? = null
    private var inviteToken: String? = null

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    // Появление сети это повод перезапросить манифест открытой офлайн-шары
    // сразу, не дожидаясь очередного шага бэкофа (XR-099). Для приложения под
    // собственным VPN дефолтная сеть это туннель, но и он поднимается следом
    // за физическим аплинком, так что сигнал годится в обоих режимах.
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            // Колбэк приходит на потоке ConnectivityManager; вся работа с
            // offlineRetryJob идёт на main, чтобы не гоняться с open/close.
            viewModelScope.launch {
                val st = _ui.value
                if (st.openShareId != null && st.offlineLocal && !st.manifestLoading) {
                    // Секунда на устаканивание маршрутов после появления сети.
                    startOfflineRetry(firstDelayMs = 1_000)
                }
            }
        }
    }

    init {
        viewModelScope.launch {
            val store = store()
            _ui.update {
                it.copy(
                    storeReady = true,
                    sortOrder = store.sortOrder(),
                    unviewedOnly = store.unviewedOnly(),
                    grouping = store.grouping(),
                )
            }
            store.shares.collect { _configs.value = it }
        }
        (app.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager)?.let { cm ->
            try {
                cm.registerDefaultNetworkCallback(networkCallback)
            } catch (_: RuntimeException) {
                // Без колбэка перезапрос держится на одном бэкофе.
            }
        }
    }

    override fun onCleared() {
        (getApplication<Application>().getSystemService(Context.CONNECTIVITY_SERVICE)
            as? ConnectivityManager)?.let { cm ->
            try { cm.unregisterNetworkCallback(networkCallback) } catch (_: Exception) {}
        }
        super.onCleared()
    }

    fun consumeMessage() = _ui.update { it.copy(message = null) }
    fun consumeOpenEvent() = _ui.update { it.copy(openFileEvent = null) }

    // -- share list --------------------------------------------------

    fun refreshHub(hubUrl: String?, inviteToken: String?) {
        this.hubUrl = hubUrl
        this.inviteToken = inviteToken
        if (hubUrl.isNullOrBlank() || inviteToken.isNullOrBlank()) {
            _ui.update { it.copy(message = text(R.string.files_no_invite)) }
            return
        }
        _ui.update { it.copy(loadingHub = true) }
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) { repo.inviteShares(hubUrl, inviteToken) }
            // Must never block clearing the spinner: a failure here would otherwise
            // leave `loadingHub` stuck true and the indicator spinning forever.
            result.onSuccess { runCatching { reconcileShares(it) } }
            _ui.update { st ->
                result.fold(
                    onSuccess = { st.copy(hubShares = it, loadingHub = false, hubOffline = false) },
                    onFailure = {
                        // No network (airplane mode, hub down): the saved shares
                        // still work, so mark the list stale instead of toasting
                        // an error over usable content (XR-093). A non-network
                        // failure is a real answer (expired invite, bad hub) and
                        // stays visible.
                        if (it.isOffline()) st.copy(loadingHub = false, hubOffline = true)
                        else st.copy(
                            loadingHub = false,
                            message = text(R.string.files_hub_shares_error, humanError(it.message)),
                        )
                    },
                )
            }
        }
    }

    /** The native layer reports transport-level failures as "network: ..."
     *  (no route, DNS, connect timeout); everything else came from the hub. */
    private fun Throwable.isOffline(): Boolean = message?.startsWith("network") == true

    /** The relay reported the share's agent gone from its registry (XR-134):
     *  the share is dead until its owner brings the agent back. An
     *  authoritative verdict, unlike the transport-level "network: ...". */
    private fun Throwable.isAgentOffline(): Boolean = message?.startsWith("agent_offline") == true

    /** The stored token no longer parses: a share added before scopes (XR-139)
     *  holds a pre-scope token. Not an error to show, a cue to refresh the grant
     *  and retry (XR-167). */
    private fun Throwable.isStaleToken(): Boolean = message?.startsWith("stale_token") == true

    /** The share fell off the invite (or the invite itself is gone): the token
     *  will never refresh, so the access is over (XR-167). Synthesized by
     *  [fetchManifestHealing] after a failed heal, never returned by the agent. */
    private fun Throwable.isAccessExpired(): Boolean = message?.startsWith("access_expired") == true

    /** Раскладка категории в вариант живёт в [ShareErrorText.kt] отдельным
     *  файлом (чистая Kotlin-логика без Android SDK, XR-243), а ресурс под
     *  вариант подставляет [renderShareError]. Пустая причина бывает у
     *  нативного вызова, который отдал только код возврата. */
    private fun humanError(e: String?): String =
        if (e.isNullOrBlank()) text(R.string.share_error_unknown)
        else renderShareError(shareErrorOf(e), appContext)

    /** То же для ошибки импорта по URL (LLD-29): разбор в [importErrorOf],
     *  ресурс под вариант в [renderShareError]. */
    private fun humanImport(e: String?): String =
        if (e.isNullOrBlank()) text(R.string.share_error_unknown)
        else renderShareError(importErrorOf(e), appContext)

    /** Текст ресурса приложения: в этой вью-модели строк UI больше нет, они
     *  живут в `strings.xml` (XR-092). */
    private fun text(id: Int, vararg args: Any): String = appContext.getString(id, *args)

    private val appContext: Context get() = getApplication<Application>()

    /** Ответ агента похож на переходное состояние (5xx, битое тело): офлайн-
     *  цикл вправе повторить попытку, как и настоящую недоступность сети
     *  (XR-243). Правило в [isTransientAgentErrorCategory]. */
    private fun Throwable.isTransientAgentError(): Boolean =
        message?.let(::isTransientAgentErrorCategory) == true

    /**
     * Refresh of the invite carries the agent's current address/port/token. If a
     * share was added earlier and the agent has since moved (e.g. a private LAN
     * address replaced by the public IP), update the stored connection fields in
     * place so we stop hitting the stale address. The user's selection and sync
     * toggle are kept; a remove + re-add is no longer needed.
     */
    private suspend fun reconcileShares(grants: List<ShareGrant>) {
        val store = store()
        grants.forEach { g ->
            val existing = store.get(g.shareId) ?: return@forEach
            if (existing.addr != g.addr || existing.addrs != g.addrs || existing.port != g.port ||
                existing.agentPubkey != g.agentPubkey || existing.tokenJson != g.tokenJson ||
                existing.relayJson != g.relayJson
            ) {
                store.update(g.shareId) {
                    it.copy(
                        addr = g.addr, port = g.port, agentPubkey = g.agentPubkey,
                        tokenJson = g.tokenJson, name = g.name,
                        // Carry the grant's extra addresses (XR-050): a share added
                        // before the hub served them, or whose LAN address changed
                        // (DHCP), must pick up the new list on refresh. Without this
                        // the stored share keeps the public address alone and stays
                        // unreachable from inside the agent's own LAN, where there
                        // is no hairpin.
                        addrs = g.addrs,
                        // Carry the grant's relay leg (XR-103): a share that became
                        // relay-reachable after it was first added must gain (or
                        // lose) its relay fallback on refresh, not only on re-add.
                        relayJson = g.relayJson,
                    )
                }
            }
        }
    }

    /** Outcome of trying to heal a share whose stored token is stale (XR-167). */
    private sealed interface StaleRecovery {
        /** The grant was refreshed (token rewritten in the store): retry the fetch. */
        object Refreshed : StaleRecovery
        /** The share is no longer on the invite, or the invite itself is gone:
         *  the token will never refresh, access is over. */
        object Gone : StaleRecovery
        /** The hub was unreachable, so nothing could be refreshed: treat as offline. */
        object Offline : StaleRecovery
    }

    /** A pre-scope token (XR-139) can only be healed by a fresh grant, so pull the
     *  invite's shares and reconcile (which rewrites the stored token in place),
     *  then report whether the share can retry, fell off the invite, or the hub
     *  was out of reach (XR-167). */
    private suspend fun recoverStaleToken(shareId: String): StaleRecovery {
        val hub = hubUrl
        val invite = inviteToken
        if (hub.isNullOrBlank() || invite.isNullOrBlank()) return StaleRecovery.Gone
        return withContext(Dispatchers.IO) { repo.inviteShares(hub, invite) }.fold(
            onSuccess = { grants ->
                reconcileShares(grants)
                _ui.update { it.copy(hubShares = grants, hubOffline = false) }
                if (grants.any { it.shareId == shareId }) StaleRecovery.Refreshed
                else StaleRecovery.Gone
            },
            onFailure = { e ->
                if (e.isOffline()) {
                    _ui.update { it.copy(hubOffline = true) }
                    StaleRecovery.Offline
                } else {
                    // The hub answered (invite expired or revoked): access is gone,
                    // not a passing outage.
                    StaleRecovery.Gone
                }
            },
        )
    }

    /** Fetch the manifest, transparently healing an old pre-scope token (XR-167):
     *  on a stale-token error refresh the invite's grant once and retry. A share
     *  that fell off the invite comes back as [ERR_ACCESS_EXPIRED], an unreachable
     *  hub as an offline error; neither ever surfaces the raw serde text. */
    private suspend fun fetchManifestHealing(config: ShareConfig): Result<List<ManifestEntry>> {
        val first = withContext(Dispatchers.IO) { repo.fetchManifest(config) }
        if (first.exceptionOrNull()?.isStaleToken() != true) return first
        return when (recoverStaleToken(config.shareId)) {
            StaleRecovery.Refreshed -> {
                val fresh = store().get(config.shareId) ?: config
                val retry = withContext(Dispatchers.IO) { repo.fetchManifest(fresh) }
                // Still stale after a fresh grant means the grant carried no usable
                // token either: access is over, not a transient parse blip.
                if (retry.exceptionOrNull()?.isStaleToken() == true) accessExpired() else retry
            }
            StaleRecovery.Gone -> accessExpired()
            StaleRecovery.Offline -> Result.failure(IllegalStateException(ERR_HUB_OFFLINE))
        }
    }

    private fun accessExpired(): Result<List<ManifestEntry>> =
        Result.failure(IllegalStateException(ERR_ACCESS_EXPIRED))

    fun addShare(grant: ShareGrant) {
        viewModelScope.launch {
            store().upsert(ShareConfig.fromGrant(grant))
            _ui.update { it.copy(message = text(R.string.files_share_added, grant.name)) }
        }
    }

    // Отметки просмотра снятой шары (XR-251): из хранилища они уходят вместе с
    // ней, иначе копились бы там от каждой ротации инвайтов, но до истечения
    // undo-снекбара лежат здесь, чтобы отмена возвращала шару целиком.
    private var droppedViewed: Pair<String, Set<String>>? = null

    /** Отмена удаления из undo-снекбара: вернуть придержанный конфиг как был,
     *  вместе с выбором, тумблером синка и отметками просмотра (локальные файлы
     *  удаление не трогает). removeShare успел отменить живую закачку, поэтому
     *  включённый синк продолжает сразу, не дожидаясь периодического слота
     *  WorkManager. */
    fun restoreShare(cfg: ShareConfig) {
        viewModelScope.launch {
            store().upsert(cfg)
            droppedViewed?.let { (id, paths) ->
                if (id == cfg.shareId) store().setViewed(id, paths)
            }
            droppedViewed = null
            rescheduleIfNeeded()
            if (cfg.syncEnabled) {
                withContext(Dispatchers.IO) { ShareSyncScheduler.syncNow(getApplication()) }
            }
        }
    }

    fun removeShare(shareId: String) {
        viewModelScope.launch {
            droppedViewed = shareId to store().viewed(shareId)
            store().remove(shareId)
            // Кэш манифеста шары больше не нужен. Undo (XR-055) вернёт запись,
            // а полный список подтянется при следующем онлайн-заходе.
            withContext(Dispatchers.IO) { repo.clearManifestCache(shareId) }
            rescheduleIfNeeded()
            // Импорты снятой шары забываются вместе с ней (XR-175): конфига под
            // них больше нет, опрос по такой строке молча ничего не меняет, а
            // цикл, обещавший гаснуть на пустом списке, крутился бы вхолостую
            // до конца жизни ViewModel.
            _ui.value.importJobs.asSequence()
                .filter { it.shareId == shareId }
                .forEach { importPollFailures.remove(it.jobId) }
            _ui.update { st ->
                st.copy(
                    queue = st.queue.filterNot { it.shareId == shareId },
                    failed = st.failed.filterNot { it.shareId == shareId },
                    importJobs = st.importJobs.filterNot { it.shareId == shareId },
                )
            }
            // A live transfer of the removed share (our head or its mirror
            // pass) would otherwise write into the dead directory for up to an
            // hour and hold the single-transfer lock the whole time.
            freshSnapshot()?.let { o ->
                if (o.optString("share") == shareId) {
                    cancelNative(o)
                }
            }
            if (_ui.value.openShareId == shareId) closeShare()
        }
    }

    // -- explorer ----------------------------------------------------

    fun openShare(config: ShareConfig) {
        offlineRetryJob?.cancel()
        _ui.update {
            it.copy(
                openShareId = config.shareId, currentPath = "",
                manifest = emptyList(), manifestLoading = true,
                offlineLocal = false, offlineFullListing = false,
                viewedPaths = emptySet(),
            )
        }
        ensureTransferPolling()
        viewModelScope.launch {
            // Отметки просмотра нужны первой же отрисовке строк, поэтому едут
            // раньше и кэша, и манифеста.
            val viewed = store().viewed(config.shareId)
            _ui.update { st ->
                if (st.openShareId != config.shareId) st else st.copy(viewedPaths = viewed)
            }
            // Cache-first (XR-059): показать что-то мгновенно, а свежий манифест
            // заменит это, когда фетч дойдёт. Раньше кэшем служил только список
            // скачанных файлов, и повторный заход офлайн ронял шару до «видно
            // только то, что на телефоне» (XR-099). Теперь предпочитаем полный
            // сохранённый манифест: офлайн видно всё содержимое шары, а пометка
            // говорит, что список последний известный. Fallback на локальные
            // файлы остаётся, когда кэша манифеста ещё нет.
            val localManifest = withContext(Dispatchers.IO) { repo.localManifest(config) }
            val local = localManifest.map { it.path }.toSet()
            adoptLocalIntoSelection(config.shareId, local)
            val cached = withContext(Dispatchers.IO) { repo.loadManifestCache(config) }
            val cachedListing = cached?.takeIf { it.isNotEmpty() }?.let { withLocalOnly(it, localManifest) }
            when {
                cachedListing != null -> _ui.update { st ->
                    if (st.openShareId != config.shareId) return@update st
                    st.copy(
                        manifest = cachedListing, manifestLoading = false,
                        localPaths = local, offlineLocal = true, offlineFullListing = true,
                    )
                }
                localManifest.isNotEmpty() -> _ui.update { st ->
                    if (st.openShareId != config.shareId) return@update st
                    st.copy(
                        manifest = localManifest, manifestLoading = false,
                        localPaths = local, offlineLocal = true, offlineFullListing = false,
                    )
                }
            }
            val result = fetchManifestHealing(config)
            _ui.update { st ->
                if (st.openShareId != config.shareId) return@update st
                result.fold(
                    onSuccess = {
                        st.copy(
                            manifest = withLocalOnly(it, localManifest),
                            manifestLoading = false, localPaths = local,
                            offlineLocal = false, offlineFullListing = false,
                        )
                    },
                    onFailure = { e ->
                        when {
                            // The agent is gone from the relay: mark the share
                            // offline and say so, instead of the raw network
                            // error against the loopback address (XR-134). Кэш
                            // манифеста (если был) остаётся на экране.
                            e.isAgentOffline() ->
                                st.copy(
                                    manifestLoading = false, offlineLocal = true,
                                    message = humanError(e.message)
                                        .replaceFirstChar { c -> c.uppercaseChar() },
                                )
                            // The stored token was pre-scope and the grant refresh
                            // proved the access is gone: say so plainly, keep any
                            // downloaded files viewable, never show serde (XR-167).
                            e.isAccessExpired() ->
                                st.copy(
                                    manifestLoading = false, offlineLocal = true,
                                    message = humanError(e.message),
                                )
                            // Ответ агента похож на переходный сбой (5xx,
                            // битое тело): ведём себя как при офлайне, чтобы
                            // список сам подтянулся фоновым ретраем (XR-243).
                            e.isTransientAgentError() ->
                                st.copy(
                                    manifestLoading = false, offlineLocal = true,
                                    message = text(
                                        R.string.files_manifest_error,
                                        humanError(e.message),
                                    ),
                                )
                            // The agent answered (expired token, http_4xx): a real
                            // error the user should see, unlike a mere no-network.
                            !e.isOffline() ->
                                st.copy(
                                    manifestLoading = false,
                                    message = text(
                                        R.string.files_manifest_error,
                                        humanError(e.message),
                                    ),
                                )
                            // Offline, а cache-first уже показал кэш или локальные
                            // файлы: пометка «Офлайн» всё говорит, тост не нужен.
                            st.manifest.isNotEmpty() -> st.copy(manifestLoading = false)
                            // Offline and nothing downloaded: an honest empty
                            // state instead of a raw error (rendered in-place).
                            else -> st.copy(manifestLoading = false, offlineLocal = true)
                        }
                    },
                )
            }
            if (result.isSuccess) {
                result.getOrNull()?.let { fresh ->
                    withContext(Dispatchers.IO) { repo.saveManifestCache(config, fresh) }
                }
                enqueueMissingSelected(config.shareId)
            } else {
                maybeStartOfflineRetry(result.exceptionOrNull())
            }
        }
    }

    /** Re-fetch the open share's listing (the explicit refresh action, split off
     *  the old sync button that confused both meanings, XR-044). Local files do
     *  not change on a listing fetch, so [UiState.localPaths] is kept as is. */
    fun refreshManifest(config: ShareConfig) {
        _ui.update { it.copy(manifestLoading = true) }
        viewModelScope.launch {
            val result = fetchManifestHealing(config)
            val localManifest = withContext(Dispatchers.IO) { repo.localManifest(config) }
            _ui.update { st ->
                if (st.openShareId != config.shareId) return@update st
                result.fold(
                    onSuccess = {
                        st.copy(
                            manifest = withLocalOnly(it, localManifest),
                            manifestLoading = false, offlineLocal = false, offlineFullListing = false,
                        )
                    },
                    onFailure = { e ->
                        // Сетевой провал явного refresh переводит шару в офлайн.
                        // Держим полный список, если он уже на экране: либо это
                        // кэш (offlineFullListing), либо свежий серверный список
                        // с онлайн-открытия (!offlineLocal). st.manifest в любой
                        // ветке не беднее localManifest, поэтому падать на него
                        // надо только когда полного списка не было вовсе, иначе
                        // жест-refresh офлайн ронял бы шару до «только скачанные»
                        // (XR-099). Ретрай молча вернёт свежий список с сетью.
                        val offline = e.isOffline() || e.isAgentOffline() || e.isTransientAgentError()
                        val hadFull = st.offlineFullListing || !st.offlineLocal
                        val fallback = if (hadFull) st.manifest else localManifest
                        st.copy(
                            manifestLoading = false,
                            manifest = if (offline) fallback else st.manifest,
                            offlineLocal = st.offlineLocal || offline,
                            offlineFullListing = if (offline) hadFull else st.offlineFullListing,
                            // No network (also the offline outcome of a stale-token
                            // heal, XR-167): a clean line, not the raw reqwest text.
                            message = if (e.isOffline()) text(R.string.files_listing_hub_offline)
                            else text(R.string.files_listing_error, humanError(e.message)),
                        )
                    },
                )
            }
            if (result.isSuccess) {
                result.getOrNull()?.let { fresh ->
                    withContext(Dispatchers.IO) { repo.saveManifestCache(config, fresh) }
                }
                enqueueMissingSelected(config.shareId)
            } else {
                maybeStartOfflineRetry(result.exceptionOrNull())
            }
        }
    }

    /** Cache-first (XR-059) стрелял fetchManifest один раз при openShare: упал
     *  запрос, и до перезахода в шару новых попыток не было, вернувшаяся сеть
     *  ничего не меняла (XR-099). Теперь неудача по сети или по молчащему
     *  агенту заводит тихий перезапрос по бэкофу; появление сети (колбэк в
     *  init) перезапускает его немедленно, а временный сбой агента (5xx,
     *  битое тело) заводит цикл тем же путём (XR-243). Ответ агента по
     *  существу (404, 4xx, подпись манифеста не сошлась) ретраем не лечится
     *  и цикл не заводит. */
    private fun maybeStartOfflineRetry(error: Throwable?) {
        if (error == null) return
        if (error.isOffline() || error.isAgentOffline() || error.isTransientAgentError()) startOfflineRetry()
    }

    private var offlineRetryJob: Job? = null

    private fun startOfflineRetry(firstDelayMs: Long = OFFLINE_RETRY_DELAYS_MS.first()) {
        offlineRetryJob?.cancel()
        offlineRetryJob = viewModelScope.launch {
            var attempt = 0
            var delayMs = firstDelayMs
            while (true) {
                delay(delayMs)
                val st = _ui.value
                val shareId = st.openShareId ?: return@launch
                if (!st.offlineLocal || st.manifestLoading) return@launch
                val config = store().get(shareId) ?: return@launch
                val result = fetchManifestHealing(config)
                if (result.isSuccess) {
                    applyFreshManifest(config, result.getOrDefault(emptyList()))
                    return@launch
                }
                // Содержательный ответ (404, 4xx, истёкший доступ, подпись не
                // сошлась) ретрай не лечит, а stale_token-ветка при нём ещё и
                // ходила бы в хаб на каждом круге: продолжаем на сетевых
                // исходах и на временном сбое агента (5xx, битое тело, XR-243).
                val e = result.exceptionOrNull()
                if (e == null || !(e.isOffline() || e.isAgentOffline() || e.isTransientAgentError())) return@launch
                attempt++
                delayMs = OFFLINE_RETRY_DELAYS_MS[minOf(attempt, OFFLINE_RETRY_DELAYS_MS.lastIndex)]
            }
        }
    }

    /** Свежий манифест пришёл вне явного действия пользователя: молча подменить
     *  список, снять пометку офлайна и доложить недокачанное в очередь. Текущая
     *  папка живёт в currentPath и подмену переживает. */
    private suspend fun applyFreshManifest(config: ShareConfig, fresh: List<ManifestEntry>) {
        val localManifest = withContext(Dispatchers.IO) { repo.localManifest(config) }
        withContext(Dispatchers.IO) { repo.saveManifestCache(config, fresh) }
        _ui.update { st ->
            if (st.openShareId != config.shareId) return@update st
            st.copy(
                manifest = withLocalOnly(fresh, localManifest),
                manifestLoading = false, offlineLocal = false, offlineFullListing = false,
            )
        }
        enqueueMissingSelected(config.shareId)
    }

    /** The server manifest plus local files it no longer lists: without them a
     *  file deleted on the server (or downloaded into a share whose agent since
     *  dropped it) would have no row, so no minus to reclaim the space when
     *  background sync is off (XR-044). */
    private fun withLocalOnly(fresh: List<ManifestEntry>, localManifest: List<ManifestEntry>): List<ManifestEntry> {
        val known = fresh.asSequence().map { it.path }.toHashSet()
        val extra = localManifest.filter { it.path !in known }
        return if (extra.isEmpty()) fresh else (fresh + extra).sortedBy { it.path }
    }

    /**
     * Fold the files already on disk into the share's selection (one-time
     * migration to the single axis, XR-044): before the redesign a file could be
     * downloaded without being selected, and the redesigned rows derive their
     * state from the disk + queue alone. Anything on the device counts as
     * wanted there, so the mirror must not prune it. Idempotent, folder
     * selections already covering a path win over a per-file entry.
     */
    private suspend fun adoptLocalIntoSelection(shareId: String, local: Set<String>) {
        if (local.isEmpty()) return
        val cfg = store().get(shareId) ?: return
        val missing = local.filterNot { isSelected(it, cfg.selection) }
        if (missing.isEmpty()) return
        store().update(shareId) { it.copy(selection = it.selection + missing) }
    }

    /** Queue every manifest file the selection wants that is neither on disk
     *  nor already queued. Makes persisted intent visible as queue rows: a
     *  queue lost to process death is rebuilt from the selection, and a
     *  pre-redesign tick that never synced surfaces instead of downloading
     *  silently later (XR-044). */
    private suspend fun enqueueMissingSelected(shareId: String) {
        val cfg = store().get(shareId) ?: return
        val st = _ui.value
        if (st.openShareId != shareId) return
        val queued = st.queue.asSequence()
            .filter { it.shareId == shareId }.map { it.entry.path }.toHashSet()
        val missing = st.manifest.filter { e ->
            isSelected(e.path, cfg.selection) && e.path !in st.localPaths && e.path !in queued
        }
        if (missing.isEmpty()) return
        val missingPaths = missing.asSequence().map { it.path }.toHashSet()
        _ui.update { s ->
            s.enqueued(missing.map { QueueItem(shareId, it) }).copy(
                failed = s.failed.filterNot { it.shareId == shareId && it.path in missingPaths },
            )
        }
        ensureQueueRunning()
    }

    /** Хвост очереди плюс новые элементы (XR-056). Батч начинается только с
     *  пустой очереди, поэтому счётчик пройденных обнуляется именно здесь:
     *  очередь пустеет и по отмене, и по удалению шары, а размер батча общий
     *  индикатор считает от него. */
    private fun UiState.enqueued(items: List<QueueItem>): UiState =
        copy(queue = queue + items, queueDone = if (queue.isEmpty()) 0 else queueDone)

    fun closeShare() {
        // Импорты закрытие шары не забывает (XR-175): опрос идёт по shareId
        // джобы, поэтому возврат в шару застаёт свои строки на месте, а уход в
        // соседнюю их не роняет. Скачивание на агенте от нас и так не зависит.
        offlineRetryJob?.cancel()
        _ui.update {
            it.copy(
                openShareId = null, currentPath = "", manifest = emptyList(),
                localPaths = emptySet(), viewedPaths = emptySet(),
                importDialogFor = null, detailsPath = null,
            )
        }
    }

    fun navigateTo(path: String) = _ui.update { it.copy(currentPath = path) }

    /** Экран информации о файле (XR-257). Открывается долгим тапом по любой
     *  строке и обычным тапом по нескачанному файлу. */
    fun openDetails(path: String) = _ui.update { it.copy(detailsPath = path) }

    fun closeDetails() = _ui.update { it.copy(detailsPath = null) }

    /** Тап по переключателю сортировки (XR-251): тот же режим разворачивает
     *  направление, другой переключает поле и берёт своё привычное начало.
     *  Выбор ложится в стор, поэтому переживает перезапуск и одинаков во всех
     *  шарах. */
    fun setSort(mode: FileSort) {
        val current = _ui.value.sortOrder
        val order = if (current.mode == mode) current.copy(descending = !current.descending)
        else SortOrder.of(mode)
        _ui.update { it.copy(sortOrder = order) }
        viewModelScope.launch { store().setSortOrder(order) }
    }

    /** Фильтр непросмотренных из меню вида (XR-256). Выбор ложится в стор рядом
     *  с сортировкой: он про привычку смотреть медиатеку, а не про открытую
     *  сейчас шару. */
    fun setUnviewedOnly(on: Boolean) {
        _ui.update { it.copy(unviewedOnly = on) }
        viewModelScope.launch { store().setUnviewedOnly(on) }
    }

    /** Режим группировки из того же меню вида (XR-258). Порядка строк он не
     *  трогает: сортировка работает внутри группы и живёт своей кнопкой. */
    fun setGrouping(mode: FileGrouping) {
        _ui.update { it.copy(grouping = mode) }
        viewModelScope.launch { store().setGrouping(mode) }
    }

    /** Кнопка «Назад» и системный жест: сначала закрывается экран истории, он
     *  лежит поверх экрана информации о файле, потом сам экран информации, он
     *  лежит поверх открытой папки, и только потом идёт подъём по дереву. */
    fun navigateUp() {
        if (_ui.value.historyPath != null) {
            closeHistory()
            return
        }
        if (_ui.value.detailsPath != null) {
            closeDetails()
            return
        }
        val p = _ui.value.currentPath
        if (p.isEmpty()) closeShare()
        else _ui.update { it.copy(currentPath = p.substringBeforeLast('/', "")) }
    }

    /** Tap a downloaded row: hand the local file to a viewer app. A file that
     *  vanished from disk (deleted via a file manager) flips its row back to
     *  the plus instead of ignoring the tap. Отданный вьюеру файл считается
     *  просмотренным (XR-251): отметка ложится в стор до показа, потому что
     *  дальше файл уходит в чужое приложение и чем там кончилось, мы не узнаем.
     *  Пишем с главного потока, как и остальные мутации стора: отметки лежат
     *  одним блобом, и два открытия подряд с пула потоков потеряли бы одну. */
    fun openLocal(config: ShareConfig, entry: ManifestEntry) {
        viewModelScope.launch {
            val existing = withContext(Dispatchers.IO) { localFile(config, entry.path) }
            if (existing != null) {
                store().markViewed(config.shareId, entry.path)
                _ui.update { st ->
                    st.copy(
                        openFileEvent = existing,
                        viewedPaths = if (st.openShareId == config.shareId) st.viewedPaths + entry.path
                        else st.viewedPaths,
                    )
                }
            } else {
                _ui.update { st ->
                    st.copy(
                        localPaths = if (st.openShareId == config.shareId) st.localPaths - entry.path else st.localPaths,
                        message = text(R.string.files_file_gone_locally),
                    )
                }
            }
        }
    }

    /** Hits the filesystem (destDir does mkdirs), so call from Dispatchers.IO. */
    private fun localFile(config: ShareConfig, relPath: String): File? =
        repo.fileFor(config, relPath).takeIf { it.isFile }

    // -- download queue (XR-044) -------------------------------------

    /** The per-row plus (and the retry after a failure): mark the file wanted
     *  and put it at the tail of the download queue. Several taps queue several
     *  files; nothing is silently dropped any more. */
    fun enqueue(config: ShareConfig, entry: ManifestEntry) {
        viewModelScope.launch {
            // A download writes, so settle where it lands first (only on the
            // very first download of this share, XR-043).
            if (!config.storageChosen) {
                promptStorage(config.shareId, Pending.Enqueue(config.shareId, entry))
                return@launch
            }
            // Selection lands before the download starts: a mirror pass running
            // in between would prune a file that is not selected yet.
            applySelection(config.shareId, entry.path, true)
            _ui.update { st ->
                if (st.queue.any { it.matches(config.shareId, entry.path) }) st
                else st.enqueued(listOf(QueueItem(config.shareId, entry))).copy(
                    failed = st.failed.filterNot { it.shareId == config.shareId && it.path == entry.path },
                )
            }
            ensureQueueRunning()
        }
    }

    /** Folder tap on a not-fully-present folder: select it and queue whatever
     *  is missing ("докачать недостающее", the Drive/Dropbox convention). */
    fun downloadFolder(config: ShareConfig, path: String) {
        viewModelScope.launch {
            if (!config.storageChosen) {
                promptStorage(config.shareId, Pending.EnqueueFolder(config.shareId, path))
                return@launch
            }
            applySelection(config.shareId, path, true)
            val prefix = "$path/"
            val st = _ui.value
            val queued = st.queue.asSequence()
                .filter { it.shareId == config.shareId }.map { it.entry.path }.toHashSet()
            val missing = st.manifest.filter { e ->
                e.path.startsWith(prefix) && e.path !in st.localPaths && e.path !in queued
            }
            _ui.update { s ->
                s.enqueued(missing.map { QueueItem(config.shareId, it) }).copy(
                    failed = s.failed.filterNot { it.shareId == config.shareId && it.path.startsWith(prefix) },
                )
            }
            ensureQueueRunning()
        }
    }

    /** Folder tap on a fully-present folder: unselect the subtree and remove its
     *  local copies (files stay on the server). */
    fun removeFolder(config: ShareConfig, path: String) {
        viewModelScope.launch {
            val prefix = "$path/"
            _ui.update { st ->
                st.copy(queue = st.queue.filterNot {
                    it.shareId == config.shareId && it.entry.path.startsWith(prefix)
                })
            }
            // Abort the share's native writer before deleting: our head under
            // the folder, or the mirror's multi-file pass, whose plan was built
            // from the old selection and would re-create files under the folder
            // right after the delete. A single-file transfer elsewhere in this
            // share is a queued row of another folder and stays.
            freshSnapshot()?.let { o ->
                if (o.optString("share") == config.shareId &&
                    (o.optString("file").startsWith(prefix) || o.optLong("files_total") > 1)
                ) {
                    cancelNative(o)
                }
            }
            awaitWriterLeft(config.shareId) { file, filesTotal ->
                file.startsWith(prefix) || filesTotal > 1
            }
            deselectPath(config.shareId, path)
            // Offline the selection may keep covering the folder (see
            // deselectPath); warn like the file minus does.
            val stillWanted = store().get(config.shareId)
                ?.let { isSelected(path, it.selection) } == true
            val local = withContext(Dispatchers.IO) {
                repo.deleteLocalUnder(config, path)
                repo.localPaths(config)
            }
            _ui.update { st ->
                st.copy(
                    localPaths = if (st.openShareId == config.shareId) local else st.localPaths,
                    failed = st.failed.filterNot { it.shareId == config.shareId && it.path.startsWith(prefix) },
                    message = if (stillWanted) text(R.string.files_offline_folder_wanted) else st.message,
                )
            }
        }
    }

    /** The per-row minus on a downloaded file: unselect it and delete the local
     *  copy (the server keeps the file; the plus brings it back). */
    fun removeLocal(config: ShareConfig, entry: ManifestEntry) {
        viewModelScope.launch {
            deselectPath(config.shareId, entry.path)
            // Offline the selection may keep covering the path (see
            // deselectPath); say so, or the file's silent return with the next
            // mirror pass would look like a bug.
            val stillWanted = store().get(config.shareId)
                ?.let { isSelected(entry.path, it.selection) } == true
            withContext(Dispatchers.IO) { repo.deleteLocal(config, entry.path) }
            _ui.update { st ->
                st.copy(
                    localPaths = if (st.openShareId == config.shareId) st.localPaths - entry.path else st.localPaths,
                    message = if (stillWanted) text(R.string.files_offline_file_wanted) else st.message,
                )
            }
        }
    }

    /**
     * Удалить файл из самой шары (LLD-28, XR-250), а не только с телефона: агент
     * убирает его у себя, и файл пропадает у всех держателей шары. Отсюда и
     * подтверждение на экране, и отдельные от минуса слова: минус в строке
     * снимает локальную копию, это действие необратимо.
     *
     * Порядок такой: сначала снимаем файл с закачки (иначе живая загрузка
     * дописала бы `.part` уже удалённого файла), потом удаляем у агента, и
     * только по успеху убираем локальную копию и строку из списка. Отказ
     * оставляет всё как было, файл остаётся и в шаре, и на устройстве.
     *
     * Экран информации открывается и для строки, которая качается прямо сейчас,
     * поэтому одной отмены мало: `nativeCancelTransfer` просит писателя встать
     * на следующем чанке, и без ожидания он допишет и переименует файл уже
     * после нашей чистки, оставив на диске копию удалённого из шары файла.
     * Дожидаемся ухода писателя ровно так же, как это делает [removeFolder].
     */
    fun deleteFromShare(config: ShareConfig, entry: ManifestEntry) {
        viewModelScope.launch {
            _ui.update { st ->
                st.copy(queue = st.queue.filterNot { it.matches(config.shareId, entry.path) })
            }
            maybeCancelNative(config.shareId, entry.path)
            val result = withContext(Dispatchers.IO) { repo.deleteRemote(config, entry) }
            result.fold(
                onSuccess = {
                    deselectPath(config.shareId, entry.path)
                    // Пока шёл сетевой запрос, за файл мог взяться фоновый синк
                    // (его план строился по манифесту, где файл ещё был), так
                    // что отмену повторяем и только потом ждём.
                    maybeCancelNative(config.shareId, entry.path)
                    awaitWriterLeft(config.shareId) { file, _ -> file == entry.path }
                    withContext(Dispatchers.IO) { repo.deleteLocal(config, entry.path) }
                    _ui.update { st ->
                        st.copy(
                            manifest = if (st.openShareId == config.shareId) {
                                st.manifest.filterNot { it.path == entry.path }
                            } else {
                                st.manifest
                            },
                            localPaths = st.localPaths - entry.path,
                            failed = st.failed.filterNot {
                                it.shareId == config.shareId && it.path == entry.path
                            },
                            message = text(R.string.files_file_deleted),
                        )
                    }
                    // Свежий манифест от агента заодно перезапишет кэш листинга,
                    // иначе удалённый файл вернулся бы строкой при офлайн-заходе.
                    refreshManifest(config)
                },
                onFailure = { e ->
                    _ui.update {
                        it.copy(
                            message = text(
                                R.string.files_delete_failed,
                                humanDeleteError(e.message.orEmpty()),
                            ),
                        )
                    }
                },
            )
        }
    }

    /** Отказы записи (LLD-28) поверх общего [humanError]: у агента здесь свои
     *  коды, и «http_412» пользователю ничего не говорит. */
    private fun humanDeleteError(e: String): String = when {
        e == "not_found" -> text(R.string.share_error_delete_not_found)
        e == "http_403" -> text(R.string.share_error_delete_forbidden)
        e == "http_412" -> text(R.string.share_error_delete_changed)
        // Хвост после категории пишет агент, и он уже человеческий; голая
        // категория остаётся на наших словах, как и в [shareErrorOf].
        e.startsWith("no_write_scope") ->
            e.substringAfter(": ", "").ifBlank { text(R.string.share_error_no_write_scope) }
        else -> humanError(e)
    }

    /** Экран истории коммитов файла (LLD-33, фаза 3). Держится на пути
     *  открытого файла, как экран информации: строки приезжают одним запросом,
     *  перезагрузки по ходу просмотра нет. Отказ не закрывает экран: причина
     *  остаётся на нём, кнопка «назад» работает и без истории. */
    fun openHistory(config: ShareConfig, entry: ManifestEntry) {
        _ui.update {
            it.copy(
                historyPath = entry.path, history = emptyList(),
                historyError = null, historyLoading = true,
            )
        }
        viewModelScope.launch {
            withContext(Dispatchers.IO) { repo.gitLog(config, entry.path) }.fold(
                onSuccess = { rows -> _ui.update { it.copy(history = rows, historyLoading = false) } },
                onFailure = { e ->
                    _ui.update {
                        it.copy(historyLoading = false, historyError = humanGitError(e.message.orEmpty()))
                    }
                },
            )
        }
    }

    fun closeHistory() = _ui.update {
        it.copy(historyPath = null, history = emptyList(), historyError = null)
    }

    /** Диалог мелкой правки (LLD-33): текст файла читается целиком, потому что
     *  правка это PUT целиком. Пока текст едет, диалог показывает ожидание и
     *  без текста не даёт сохранить пустоту вместо содержимого. */
    fun openEditor(config: ShareConfig, entry: ManifestEntry) {
        _ui.update {
            it.copy(editPath = entry.path, editSha = entry.sha256, editText = "", editLoading = true)
        }
        viewModelScope.launch {
            withContext(Dispatchers.IO) { repo.readShareText(config, entry) }.fold(
                onSuccess = { content -> _ui.update { it.copy(editText = content, editLoading = false) } },
                onFailure = { e ->
                    _ui.update {
                        it.copy(
                            editPath = null, editLoading = false,
                            message = text(R.string.files_edit_load_failed, humanGitError(e.message.orEmpty())),
                        )
                    }
                },
            )
        }
    }

    fun updateEditText(s: String) = _ui.update { it.copy(editText = s) }

    fun closeEditor() = _ui.update { it.copy(editPath = null, editBusy = false) }

    /** Отправить правку в шару тем же PUT-контуром, что и страница (LLD-33).
     *  Успех обновляет манифест: хеш строки изменился, и следом открытая
     *  правка без обновления немедленно словила бы 412. Отказ 412 значит, что
     *  файл менялся у агента: правку закрываем, текст в поле уже устарел. */
    fun saveEdit(config: ShareConfig, entry: ManifestEntry) {
        val newText = _ui.value.editText
        _ui.update { it.copy(editBusy = true) }
        viewModelScope.launch {
            withContext(Dispatchers.IO) {
                repo.uploadText(config, entry.path, newText, _ui.value.editSha)
            }.fold(
                onSuccess = {
                    _ui.update {
                        it.copy(editPath = null, editBusy = false, message = text(R.string.files_edit_saved))
                    }
                    refreshManifest(config)
                },
                onFailure = { e ->
                    _ui.update {
                        it.copy(
                            editPath = null, editBusy = false,
                            message = text(R.string.files_edit_failed, humanGitError(e.message.orEmpty())),
                        )
                    }
                    // При 412 файл на агенте уже другой: без обновления каждая
                    // следующая правка билась бы в тот же отказ.
                    refreshManifest(config)
                },
            )
        }
    }

    /** Отказы git-контура и правки (LLD-33) поверх общего [humanError]:
     *  категории приходят от xr-core и агента, у приложения на каждую свои
     *  слова. */
    private fun humanGitError(e: String): String = when {
        e == "git_off" -> text(R.string.share_error_git_off)
        e == "http_412" -> text(R.string.share_error_edit_changed)
        e.startsWith("no_write_scope") ->
            e.substringAfter(": ", "").ifBlank { text(R.string.share_error_no_write_scope) }
        else -> humanError(e)
    }

    /** The cancel control of any downloading or queued row: take the file off
     *  the wanted set and the queue; abort the native transfer only when it is
     *  exactly this file. The partial stays on disk, so a later plus resumes
     *  instead of restarting. */
    fun cancelDownload(shareId: String, path: String) {
        viewModelScope.launch {
            deselectPath(shareId, path)
            val item = _ui.value.queue.firstOrNull { it.matches(shareId, path) }
            if (item != null) {
                _ui.update { st -> st.copy(queue = st.queue.filter { it !== item }) }
            }
            maybeCancelNative(shareId, path)
        }
    }

    /** Стоп всей очереди из общего индикатора (XR-056): та же отмена, что у
     *  строки файла, применённая ко всему, что стоит в очереди. Пустая очередь
     *  значит, что индикатор показывает проход фонового зеркала, и останавливать
     *  надо саму передачу; недокачанное остаётся на диске, как и при отмене
     *  одной строки. */
    fun cancelQueue() {
        val items = _ui.value.queue.toList()
        if (items.isEmpty()) {
            cancelTransfer()
            return
        }
        items.forEach { cancelDownload(it.shareId, it.entry.path) }
    }

    /** A fresh native transfer snapshot, or null when idle or unparsable. The
     *  UI one is up to 500ms stale; cancel decisions need the current owner. */
    private suspend fun freshSnapshot(): JSONObject? = withContext(Dispatchers.IO) {
        runCatching { JSONObject(NativeBridge.nativeTransferProgress()) }.getOrNull()
            ?.takeIf { it.optBoolean("active") }
    }

    /** Abort the native transfer only if it is running exactly this share's
     *  [path]: a cancel must never kill an unrelated mirror pass or a storage
     *  migration. Aborting a mirror that fetches this very file is the user's
     *  intent; the pass's remaining files return on its next cycle. */
    private suspend fun maybeCancelNative(shareId: String, path: String) {
        freshSnapshot()?.let { o ->
            if (o.optString("share") == shareId && o.optString("file") == path) {
                cancelNative(o)
            }
        }
    }

    /** Отменить ровно ту передачу, которую показал снимок [o]. Номер отмены
     *  берётся из него же: пока решение шло до нативной стороны, та передача
     *  могла закончиться, и безадресная отмена обрывала уже следующую (XR-217).
     *  Опоздавшая отмена теперь никуда не попадает и отвечает `false`. */
    private suspend fun cancelNative(o: JSONObject): Boolean {
        val id = o.optLong("id")
        return withContext(Dispatchers.IO) { NativeBridge.nativeCancelTransfer(id) }
    }

    /** Bounded wait until the native writer can no longer touch what is about to
     *  be deleted locally: [touches] judges the running transfer by its file and
     *  its file count (a multi-file pass of the same share may still reach the
     *  target). A cancelled download flushes its partial at the next chunk, so
     *  this is quick. */
    private suspend fun awaitWriterLeft(
        shareId: String,
        touches: (String, Long) -> Boolean,
    ) = withContext(Dispatchers.IO) {
        repeat(30) {
            val o = runCatching { JSONObject(NativeBridge.nativeTransferProgress()) }.getOrNull()
                ?: return@withContext
            if (!o.optBoolean("active") || o.optString("share") != shareId ||
                !touches(o.optString("file"), o.optLong("files_total"))
            ) {
                return@withContext
            }
            delay(100)
        }
    }

    private suspend fun applySelection(shareId: String, path: String, selected: Boolean) {
        store().update(shareId) { cfg ->
            val sel = cfg.selection.toMutableSet()
            sel.removeAll { it == path || it.startsWith("$path/") }
            if (selected) sel.add(path)
            cfg.copy(selection = sel)
        }
    }

    /** Unselect [path], splitting a covering folder selection into the sibling
     *  branches (in Rust, next to the mirror planner) so only this file or
     *  folder leaves the wanted set. Offline the manifest lists local files
     *  only, and a split would silently drop the invisible branches, so only
     *  direct entries are removed there. */
    private suspend fun deselectPath(shareId: String, path: String) {
        val st = _ui.value
        val offline = st.offlineLocal && st.openShareId == shareId
        val manifestPaths = if (st.openShareId == shareId) st.manifest.map { it.path } else emptyList()
        store().update(shareId) { cfg ->
            val sel =
                if (offline) cfg.selection.filterNot { it == path || it.startsWith("$path/") }.toSet()
                else repo.expandDeselect(cfg.selection, manifestPaths, path)
            cfg.copy(selection = sel)
        }
    }

    private var queueJob: Job? = null

    private fun ensureQueueRunning() {
        ensureTransferPolling()
        if (queueJob?.isActive == true) return
        queueJob = viewModelScope.launch {
            while (true) {
                val item = _ui.value.queue.firstOrNull() ?: break
                val cfg = store().get(item.shareId)
                var err: String? = null
                if (cfg == null) {
                    // The share was removed while its file sat in the queue.
                    err = "cancelled"
                } else if (withContext(Dispatchers.IO) { repo.fileFor(cfg, item.entry.path).isFile }) {
                    // Already on disk: the mirror fetched it while the file sat
                    // in the queue, a second full download would be pure waste.
                } else {
                    while (true) {
                        err = withContext(Dispatchers.IO) { repo.downloadOne(cfg, item.entry) }
                        if (err != "busy") break
                        // The background mirror holds the single-transfer lock:
                        // wait it out, unless the user took the file off the queue.
                        delay(2_000)
                        if (_ui.value.queue.none { it === item }) {
                            err = "cancelled"
                            break
                        }
                    }
                }
                val done = err == null
                // A cancel may have raced the download's own completion: an item
                // gone from the queue was cancelled by the user, not failed.
                val cancelled = err == "cancelled" || _ui.value.queue.none { it === item }
                // Saved progress of a failure comes from the resume partial on
                // disk, not the transfer snapshot: the poller may have cleared
                // the snapshot already, the partial is what the retry resumes.
                val bytesDone = if (!done && !cancelled && cfg != null) {
                    withContext(Dispatchers.IO) { repo.partialSize(cfg, item.entry.path) }
                } else {
                    0L
                }
                _ui.update { st ->
                    val rest = st.queue.filter { it !== item }
                    st.copy(
                        queue = rest,
                        queueDone = queueDoneAfter(st.queueDone, rest.size, cancelled),
                        localPaths = if (done && st.openShareId == item.shareId) st.localPaths + item.entry.path
                        else st.localPaths,
                        failed = if (done || cancelled) st.failed
                        else st.failed + FailedDownload(
                            item.shareId, item.entry.path,
                            bytesDone, item.entry.size, humanError(err),
                        ),
                        // One toast per burst: the first failure says why, the
                        // rest just turn their rows red (a dead network would
                        // otherwise queue a toast per file).
                        message = if (!done && !cancelled && st.failed.isEmpty()) {
                            text(
                                R.string.files_download_failed,
                                item.entry.path.substringAfterLast('/'),
                                humanError(err),
                            )
                        } else {
                            st.message
                        },
                    )
                }
            }
        }
    }

    // -- transfer progress on rows -----------------------------------

    private var transferPoll: Job? = null

    /** Вкладка «Файлы» на экране (XR-056). Опрос снимка держится всё это время,
     *  иначе фоновый синк виден только внутри открытой шары: список шар не
     *  открывает ни одной, очередь у него пуста, и опрос заканчивался на первом
     *  же витке, а вместе с ним пропадал и общий индикатор. */
    private var screenOpen = false

    /** Экран показан: начать следить за передачами. */
    fun watchTransfers() {
        screenOpen = true
        ensureTransferPolling()
    }

    /** Экран ушёл: опрос доживает до конца текущей работы и заканчивается. */
    fun unwatchTransfers() {
        screenOpen = false
    }

    /**
     * Poll the native transfer snapshot while the Files screen is on, the
     * explorer is open, the queue is busy or a migration runs. Опрос при
     * показанном экране это то, что делает фоновый синк видимым на списке шар
     * (XR-056). One poller covers the foreground queue, the
     * background mirror and the migration card: rows match themselves against
     * the snapshot's share + path, which is what makes background sync visible
     * per row (XR-044). When a transfer ends the local set is re-read, so
     * freshly mirrored files flip their rows without reopening the share.
     */
    private fun ensureTransferPolling() {
        if (transferPoll?.isActive == true) return
        transferPoll = viewModelScope.launch {
            var lastBytes = 0L
            var lastTime = System.currentTimeMillis()
            var lastFile = ""
            var wasActive = false
            while (screenOpen || _ui.value.openShareId != null || _ui.value.queue.isNotEmpty() ||
                _ui.value.migratingShareId != null
            ) {
                val snap = withContext(Dispatchers.IO) { NativeBridge.nativeTransferProgress() }
                var active = false
                runCatching { JSONObject(snap) }.getOrNull()?.let { o ->
                    active = o.optBoolean("active", false)
                    val bytesDone = if (active) o.optLong("bytes_done") else 0L
                    val file = o.optString("file")
                    val now = System.currentTimeMillis()
                    val dt = (now - lastTime).coerceAtLeast(1)
                    // The first sample of an already-running transfer has no
                    // previous point: its whole byte count over a few ms would
                    // read as an absurd speed, so it shows as 0 for one tick.
                    val speed = if (active && wasActive && file == lastFile) {
                        ((bytesDone - lastBytes) * 1000 / dt).coerceAtLeast(0)
                    } else {
                        0
                    }
                    lastBytes = bytesDone
                    lastTime = now
                    lastFile = file
                    _ui.update {
                        it.copy(
                            transfer = if (!active) null else Progress(
                                share = o.optString("share"),
                                file = o.optString("file"),
                                filesDone = o.optLong("files_done"),
                                filesTotal = o.optLong("files_total"),
                                bytesDone = bytesDone,
                                bytesTotal = o.optLong("bytes_total"),
                                speedBytesPerSec = speed,
                            ),
                        )
                    }
                }
                if (wasActive && !active) refreshLocal()
                wasActive = active
                // Fast only while something is moving: an idle open screen does
                // not need two JNI polls a second.
                delay(if (active || _ui.value.queue.isNotEmpty()) 500 else 1_500)
            }
            _ui.update { it.copy(transfer = null) }
        }
    }

    /** Re-read the open share's local files (after a transfer finished); a
     *  failure record whose file arrived (a mirror retry succeeded) is stale
     *  and leaves with it. */
    private suspend fun refreshLocal() {
        val id = _ui.value.openShareId ?: return
        val cfg = store().get(id) ?: return
        val local = withContext(Dispatchers.IO) { repo.localPaths(cfg) }
        _ui.update { st ->
            if (st.openShareId != id) st
            else st.copy(
                localPaths = local,
                failed = st.failed.filterNot { it.shareId == id && it.path in local },
            )
        }
    }

    // -- background mirror toggle ------------------------------------

    fun setSyncEnabled(shareId: String, enabled: Boolean) {
        viewModelScope.launch {
            val cfg = store().get(shareId) ?: return@launch
            if (enabled && !cfg.hasToken) {
                _ui.update { it.copy(message = text(R.string.files_no_token)) }
                return@launch
            }
            // Enabling background sync starts writing files, so settle the storage
            // directory first (once per share).
            if (enabled && !cfg.storageChosen) {
                promptStorage(shareId, Pending.EnableSync(shareId))
                return@launch
            }
            store().update(shareId) { it.copy(syncEnabled = enabled) }
            rescheduleIfNeeded()
            if (enabled) {
                withContext(Dispatchers.IO) { ShareSyncScheduler.syncNow(getApplication()) }
                _ui.update { it.copy(message = text(R.string.files_sync_enabled)) }
            }
        }
    }

    /** Cancel the running native transfer (the storage-migration card's stop).
     *  Номер берётся из свежего снимка, иначе останавливать нечего. */
    fun cancelTransfer() {
        viewModelScope.launch {
            freshSnapshot()?.let { cancelNative(it) }
        }
        _ui.update { it.copy(message = text(R.string.files_stopping)) }
    }

    // -- URL import (LLD-29) ----------------------------------------

    private var importPoll: Job? = null

    /** Consecutive failed polls tolerated before giving up: one lost poll on a
     *  network handover must not orphan a job that keeps running on the agent. */
    private val importPollFailureLimit = 3

    /** Промахи опроса по каждой джобе: счёт свой, иначе одна недоступная джоба
     *  снимала бы соседние. */
    private val importPollFailures = mutableMapOf<String, Int>()

    fun openImportDialog(shareId: String) = _ui.update { it.copy(importDialogFor = shareId) }
    fun dismissImportDialog() = _ui.update { it.copy(importDialogFor = null) }

    fun dismissImportError() = _ui.update { it.copy(importError = null) }

    /** Start importing [url] into the currently open folder of the share.
     *  [height] null means "Максимум": the owner's cap alone limits quality.
     *  Гейта на второй импорт нет (XR-175): очередь держит агент, а строка в
     *  списке даёт чем отследить и отменить каждую джобу. */
    fun startImport(config: ShareConfig, url: String, height: Int?) {
        val dest = _ui.value.currentPath
        _ui.update { it.copy(importDialogFor = null) }
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) {
                repo.importUrl(config, url.trim(), dest, height)
            }
            result.fold(
                onSuccess = { jobId ->
                    _ui.update { it.copy(importJobs = it.importJobs + ImportJob(config.shareId, jobId)) }
                    ensureImportPolling()
                },
                onFailure = { e ->
                    _ui.update { it.copy(importError = humanImport(e.message)) }
                },
            )
        }
    }

    /** The cross on the import row: kill the download on the agent, forget the
     *  job. Остальные джобы списка идут дальше своим чередом. */
    fun cancelImport(config: ShareConfig, jobId: String) {
        forgetImportJob(jobId)
        viewModelScope.launch(Dispatchers.IO) { repo.importCancel(config, jobId) }
    }

    private fun forgetImportJob(jobId: String) {
        importPollFailures.remove(jobId)
        _ui.update { it.copy(importJobs = it.importJobs.filterNot { job -> job.jobId == jobId }) }
    }

    /** Один цикл на весь список (XR-175): раз в 2 секунды обходит все живые
     *  джобы, свою на каждую заводить незачем. Живёт, пока список не опустеет,
     *  и переживает уход в другую шару: конфиг берётся по shareId джобы. */
    private fun ensureImportPolling() {
        if (importPoll?.isActive == true) return
        importPoll = viewModelScope.launch {
            while (_ui.value.importJobs.isNotEmpty()) {
                delay(2_000)
                for (job in _ui.value.importJobs) pollImportJob(job)
            }
            importPoll = null
        }
    }

    /** Один опрос джобы (LLD-29 п. 2.8): done убирает строку и обновляет
     *  листинг, failed показывает причину от агента. Промахи сети терпятся до
     *  [importPollFailureLimit] подряд на джобу: она качается на агенте, и один
     *  потерянный ответ о ней ничего не говорит. */
    private suspend fun pollImportJob(job: ImportJob) {
        val config = store().get(job.shareId) ?: return
        val result = withContext(Dispatchers.IO) { repo.importStatus(config, job.jobId) }
        val state = result.getOrNull()
        when {
            state == null -> {
                val failures = (importPollFailures[job.jobId] ?: 0) + 1
                importPollFailures[job.jobId] = failures
                if (failures >= importPollFailureLimit) {
                    forgetImportJob(job.jobId)
                    _ui.update {
                        it.copy(importError = humanImport(result.exceptionOrNull()?.message))
                    }
                }
            }
            state.state == "done" -> {
                forgetImportJob(job.jobId)
                _ui.update { it.copy(message = text(R.string.files_import_done)) }
                refreshOpenShare(config)
            }
            state.state == "failed" -> {
                forgetImportJob(job.jobId)
                _ui.update {
                    it.copy(importError = state.error ?: text(R.string.files_import_reason_unknown))
                }
            }
            else -> {
                importPollFailures.remove(job.jobId)
                _ui.update { st ->
                    st.copy(
                        importJobs = st.importJobs.map {
                            if (it.jobId == job.jobId) {
                                it.copy(progress = state.progress, queued = state.state == "queued")
                            } else {
                                it
                            }
                        },
                    )
                }
            }
        }
    }

    /** Re-fetch the open share's manifest after a finished import, so the new
     *  file shows up in the listing without a manual refresh. */
    private suspend fun refreshOpenShare(config: ShareConfig) {
        if (_ui.value.openShareId != config.shareId) return
        val result = withContext(Dispatchers.IO) { repo.fetchManifest(config) }
        val localManifest = withContext(Dispatchers.IO) { repo.localManifest(config) }
        _ui.update { st ->
            if (st.openShareId != config.shareId) return@update st
            result.fold(
                onSuccess = {
                    st.copy(
                        manifest = withLocalOnly(it, localManifest),
                        localPaths = localManifest.asSequence().map { e -> e.path }.toSet(),
                        offlineLocal = false,
                    )
                },
                onFailure = { st },
            )
        }
    }

    // -- storage directory (XR-043) ----------------------------------

    private fun promptStorage(shareId: String, action: Pending) {
        pending = action
        _ui.update { it.copy(storageDialogFor = shareId, storagePromptMode = true) }
    }

    /** Open the storage dialog from share settings (change folder at any time). */
    fun openStorageDialog(shareId: String) {
        pending = null
        _ui.update { it.copy(storageDialogFor = shareId, storagePromptMode = false) }
    }

    fun dismissStorageDialog() {
        pending = null
        _ui.update { it.copy(storageDialogFor = null, storagePromptMode = false) }
    }

    /** Close the dialog to show the system folder picker, keeping any deferred
     *  first-sync action so it runs once the folder is chosen. */
    fun hideStorageDialog() = _ui.update { it.copy(storageDialogFor = null) }

    /**
     * Apply a storage choice for [shareId]: [parentPath] null = the app directory,
     * non-null = a user-picked **parent** folder (the share gets its own subfolder
     * inside it, so the true-mirror delete can't touch the user's other files
     * there). When the location changes, the already-downloaded files are migrated
     * (moved, not re-downloaded) before the new path is persisted; a failed
     * migration keeps the old location. On success any deferred first-sync action
     * runs.
     */
    fun chooseStorage(shareId: String, parentPath: String?) {
        viewModelScope.launch {
            val cfg = store().get(shareId) ?: return@launch
            val newPath = parentPath?.let { File(it, repo.shareSubdir(cfg)).absolutePath }
            val newDir = repo.dirFor(newPath, shareId)
            val samePlace = withContext(Dispatchers.IO) {
                repo.destDir(cfg).absolutePath == newDir.absolutePath
            }
            if (samePlace) {
                store().update(shareId) { it.copy(storagePath = newPath, storageChosen = true) }
                _ui.update { it.copy(storageDialogFor = null, storagePromptMode = false) }
                runPending(shareId)
                return@launch
            }
            if (_ui.value.migratingShareId != null || _ui.value.queue.isNotEmpty()) {
                _ui.update { it.copy(message = text(R.string.files_transfer_busy)) }
                return@launch
            }
            _ui.update {
                it.copy(
                    storageDialogFor = null, storagePromptMode = false,
                    migratingShareId = shareId,
                )
            }
            ensureTransferPolling()
            val outcome = withContext(Dispatchers.IO) { repo.migrateStorage(cfg, newDir) }
            val persisted = outcome.ok && !outcome.cancelled
            if (persisted) store().update(shareId) { it.copy(storagePath = newPath, storageChosen = true) }
            val fresh = store().get(shareId)
            val local = if (_ui.value.openShareId == shareId && fresh != null) {
                withContext(Dispatchers.IO) { repo.localPaths(fresh) }
            } else {
                _ui.value.localPaths
            }
            _ui.update {
                it.copy(
                    migratingShareId = null, localPaths = local,
                    message = migrateMessage(outcome, newPath),
                )
            }
            if (persisted) runPending(shareId)
        }
    }

    /** Re-dispatch the action the first-sync prompt deferred, now that the share's
     *  config is updated (storageChosen = true, so it won't prompt again). */
    private suspend fun runPending(shareId: String) {
        val p = pending ?: return
        pending = null
        if (p.shareIdOf() != shareId) return
        when (p) {
            is Pending.Enqueue -> store().get(p.shareId)?.let { enqueue(it, p.entry) }
            is Pending.EnqueueFolder -> store().get(p.shareId)?.let { downloadFolder(it, p.path) }
            is Pending.EnableSync -> setSyncEnabled(p.shareId, true)
        }
    }

    private fun Pending.shareIdOf(): String = when (this) {
        is Pending.Enqueue -> shareId
        is Pending.EnqueueFolder -> shareId
        is Pending.EnableSync -> shareId
    }

    private fun migrateMessage(o: ShareRepository.MigrateOutcome, newPath: String?): String = when {
        o.error == "busy" -> text(R.string.files_migrate_busy)
        o.error?.startsWith("no_space") == true -> text(R.string.files_migrate_no_space)
        o.error != null -> text(R.string.files_migrate_failed, o.error)
        o.cancelled -> text(R.string.files_migrate_cancelled)
        else -> text(R.string.files_migrate_done, StorageAccess.label(getApplication(), newPath), o.moved) +
            (if (o.conflicts > 0) text(R.string.files_migrate_conflicts, o.conflicts) else "") +
            (if (o.failed > 0) text(R.string.files_migrate_errors, o.failed) else "")
    }

    // The scheduler goes through WorkManager, and its first getInstance in the
    // process opens a Room database. That is another main-thread stall (XR-093),
    // so both hops run on IO.

    private suspend fun rescheduleIfNeeded() = withContext(Dispatchers.IO) {
        if (store().enabledShares().isNotEmpty()) ShareSyncScheduler.schedulePeriodic(getApplication())
        else ShareSyncScheduler.cancelPeriodic(getApplication())
    }

    fun syncAllNow() {
        viewModelScope.launch(Dispatchers.IO) {
            if (store().enabledShares().isNotEmpty()) ShareSyncScheduler.syncNow(getApplication())
        }
    }

    private companion object {
        /** A stale token whose grant refresh proved the access is gone (XR-167).
         *  Голая категория без хвоста: слова к ней подбирает [shareErrorOf], а
         *  свёртки роутят её отдельно от serde (XR-092). */
        const val ERR_ACCESS_EXPIRED = "access_expired"

        /** Stale token, but the hub was unreachable to refresh the grant: reuse
         *  the "network" category so the offline handling kicks in (XR-167).
         *  Текст этой ветки на экран не выходит: офлайн-развилка говорит своими
         *  словами, а сюда смотрит только [Throwable.isOffline]. */
        const val ERR_HUB_OFFLINE = "network"

        /** Бэкоф тихого перезапроса манифеста офлайн-шары (XR-099); последний
         *  шаг повторяется, пока шара открыта. */
        val OFFLINE_RETRY_DELAYS_MS = longArrayOf(10_000, 30_000, 60_000)
    }
}
