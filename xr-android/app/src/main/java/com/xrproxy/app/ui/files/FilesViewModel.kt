package com.xrproxy.app.ui.files

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.data.ShareRepository
import com.xrproxy.app.data.ShareStore
import com.xrproxy.app.data.StorageAccess
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareGrant
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
 * Transfers report progress (polled from native) and can be cancelled.
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

    /** Live progress of the running sync/download. */
    data class Progress(
        val file: String,
        val filesDone: Long,
        val filesTotal: Long,
        val bytesDone: Long,
        val bytesTotal: Long,
        val speedBytesPerSec: Long,
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
        val localPaths: Set<String> = emptySet(),
        val busyShareId: String? = null,
        val progress: Progress? = null,
        val openFileEvent: File? = null,
        val message: String? = null,
        /** Share whose storage-directory dialog is open (XR-043), or null. */
        val storageDialogFor: String? = null,
        /** True when the dialog is the first-sync prompt (auto-continues the
         *  deferred action on choice) vs. a later "change folder" from settings. */
        val storagePromptMode: Boolean = false,
    )

    /** A sync action deferred until the user makes the first-sync storage choice. */
    private sealed interface Pending {
        data class Download(val shareId: String, val entry: ManifestEntry) : Pending
        data class Sync(val shareId: String) : Pending
        data class EnableSync(val shareId: String) : Pending
    }

    private var pending: Pending? = null

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    init {
        viewModelScope.launch {
            val store = store()
            _ui.update { it.copy(storeReady = true) }
            store.shares.collect { _configs.value = it }
        }
    }

    fun consumeMessage() = _ui.update { it.copy(message = null) }
    fun consumeOpenEvent() = _ui.update { it.copy(openFileEvent = null) }

    // ── share list ──────────────────────────────────────────────────

    fun refreshHub(hubUrl: String?, inviteToken: String?) {
        if (hubUrl.isNullOrBlank() || inviteToken.isNullOrBlank()) {
            _ui.update { it.copy(message = "Нет инвайта, добавьте сервер по инвайту") }
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
                        else st.copy(loadingHub = false, message = "Шары по инвайту: ${it.message}")
                    },
                )
            }
        }
    }

    /** The native layer reports transport-level failures as "network: ..."
     *  (no route, DNS, connect timeout); everything else came from the hub. */
    private fun Throwable.isOffline(): Boolean = message?.startsWith("network") == true

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
            if (existing.addr != g.addr || existing.port != g.port ||
                existing.agentPubkey != g.agentPubkey || existing.tokenJson != g.tokenJson
            ) {
                store.update(g.shareId) {
                    it.copy(
                        addr = g.addr, port = g.port, agentPubkey = g.agentPubkey,
                        tokenJson = g.tokenJson, name = g.name,
                    )
                }
            }
        }
    }

    fun addShare(grant: ShareGrant) {
        viewModelScope.launch {
            store().upsert(ShareConfig.fromGrant(grant))
            _ui.update { it.copy(message = "Шара «${grant.name}» добавлена") }
        }
    }

    fun removeShare(shareId: String) {
        viewModelScope.launch {
            store().remove(shareId)
            rescheduleIfNeeded()
            if (_ui.value.openShareId == shareId) closeShare()
        }
    }

    // ── explorer ────────────────────────────────────────────────────

    fun openShare(config: ShareConfig) {
        _ui.update {
            it.copy(
                openShareId = config.shareId, currentPath = "",
                manifest = emptyList(), manifestLoading = true, offlineLocal = false,
            )
        }
        viewModelScope.launch {
            // Cache-first (XR-059): the already-downloaded files show up right
            // away, the fresh manifest replaces them when the fetch lands. The
            // old fetch-then-fallback order kept a spinner up for the whole
            // manifest timeout when the VPN is up but the server unreachable:
            // the local smoltcp stack accepts the connect (SYN-ACK before the
            // upstream), so the connect-timeout never fires.
            val localManifest = withContext(Dispatchers.IO) { repo.localManifest(config) }
            val local = localManifest.map { it.path }.toSet()
            if (localManifest.isNotEmpty()) {
                _ui.update { st ->
                    if (st.openShareId != config.shareId) return@update st
                    st.copy(manifest = localManifest, manifestLoading = false, localPaths = local, offlineLocal = true)
                }
            }
            val result = withContext(Dispatchers.IO) { repo.fetchManifest(config) }
            _ui.update { st ->
                if (st.openShareId != config.shareId) return@update st
                result.fold(
                    onSuccess = {
                        st.copy(manifest = it, manifestLoading = false, localPaths = local, offlineLocal = false)
                    },
                    onFailure = { e ->
                        when {
                            // The agent answered (expired token, http_4xx): a real
                            // error the user should see, unlike a mere no-network.
                            !e.isOffline() ->
                                st.copy(manifestLoading = false, message = "Манифест: ${e.message}")
                            // Offline with the local list already on screen: the
                            // "Офлайн" mark says it all, no toast on top.
                            localManifest.isNotEmpty() -> st
                            // Offline and nothing downloaded: an honest empty
                            // state instead of a raw error (rendered in-place).
                            else -> st.copy(manifestLoading = false, offlineLocal = true)
                        }
                    },
                )
            }
        }
    }

    fun closeShare() = _ui.update {
        it.copy(openShareId = null, currentPath = "", manifest = emptyList(), localPaths = emptySet())
    }

    fun navigateTo(path: String) = _ui.update { it.copy(currentPath = path) }

    fun navigateUp() {
        val p = _ui.value.currentPath
        if (p.isEmpty()) closeShare()
        else _ui.update { it.copy(currentPath = p.substringBeforeLast('/', "")) }
    }

    fun setSelected(shareId: String, path: String, selected: Boolean) {
        viewModelScope.launch { applySelection(shareId, path, selected) }
    }

    private suspend fun applySelection(shareId: String, path: String, selected: Boolean) {
        store().update(shareId) { cfg ->
            val sel = cfg.selection.toMutableSet()
            sel.removeAll { it == path || it.startsWith("$path/") }
            if (selected) sel.add(path)
            cfg.copy(selection = sel)
        }
    }

    /** Tap a file: open it if downloaded, else download (with progress) then open.
     *  A tap is a manual sync of this file: it is added to the selection so it is
     *  treated exactly like a ticked + synced file (kept locally, re-mirrored when
     *  the toggle is on, never pruned as an ad-hoc copy). */
    fun downloadAndOpen(config: ShareConfig, entry: ManifestEntry) {
        viewModelScope.launch {
            // Selection lands before the download starts: a mirror pass running
            // in between would prune a file that is not selected yet.
            applySelection(config.shareId, entry.path, true)
            val existing = withContext(Dispatchers.IO) { localFile(config, entry.path) }
            if (existing != null) {
                _ui.update { st -> st.copy(openFileEvent = existing) }
                return@launch
            }
            // A download writes, so settle where it lands first (only on the very
            // first sync of this share); an already-downloaded file opened above.
            if (!config.storageChosen) {
                promptStorage(config.shareId, Pending.Download(config.shareId, entry))
                return@launch
            }
            if (_ui.value.busyShareId != null) return@launch
            _ui.update { it.copy(busyShareId = config.shareId, progress = preparing()) }
            val poll = launchPolling()
            val err = withContext(Dispatchers.IO) { repo.downloadOne(config, entry) }
            poll.cancel()
            val local = withContext(Dispatchers.IO) { repo.localPaths(config) }
            _ui.update {
                it.copy(
                    busyShareId = null, localPaths = local, progress = null,
                    openFileEvent = if (err == null) repo.fileFor(config, entry.path) else null,
                    message = when {
                        err == null -> null
                        err == "busy" -> "Идёт синхронизация, попробуй ещё раз"
                        else -> "Не удалось скачать"
                    },
                )
            }
        }
    }

    /** Hits the filesystem (destDir does mkdirs), so call from Dispatchers.IO. */
    private fun localFile(config: ShareConfig, relPath: String): File? =
        repo.fileFor(config, relPath).takeIf { it.isFile }

    // ── sync + transfer control ─────────────────────────────────────

    fun setSyncEnabled(shareId: String, enabled: Boolean) {
        viewModelScope.launch {
            val cfg = store().get(shareId) ?: return@launch
            if (enabled && !cfg.hasToken) {
                _ui.update { it.copy(message = "Нет токена доступа") }
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
                _ui.update { it.copy(message = "Синк включён (зеркалит выбранное, удаляет лишнее)") }
            }
        }
    }

    fun syncNow(config: ShareConfig) {
        if (config.selection.isEmpty()) {
            _ui.update { it.copy(message = "Отметь галочками файлы или папки для синка") }
            return
        }
        if (!config.storageChosen) {
            promptStorage(config.shareId, Pending.Sync(config.shareId))
            return
        }
        if (_ui.value.busyShareId != null) return
        _ui.update { it.copy(busyShareId = config.shareId, progress = preparing()) }
        val poll = launchPolling()
        viewModelScope.launch {
            val outcome = withContext(Dispatchers.IO) { repo.syncOnce(config) }
            poll.cancel()
            val local = withContext(Dispatchers.IO) { repo.localPaths(config) }
            _ui.update {
                it.copy(
                    busyShareId = null, progress = null, localPaths = local,
                    message = when {
                        outcome.ok ->
                            "Синк «${config.name}»: +${outcome.fetched} −${outcome.deleted}" +
                                if (outcome.failed > 0) " (ошибок ${outcome.failed})" else ""
                        outcome.error == "busy" -> "Идёт синхронизация, подождите"
                        else -> "Синк «${config.name}»: ${outcome.error}"
                    },
                )
            }
        }
    }

    fun cancelTransfer() {
        viewModelScope.launch { withContext(Dispatchers.IO) { NativeBridge.nativeCancelTransfer() } }
        _ui.update { it.copy(message = "Останавливаю…") }
    }

    // ── storage directory (XR-043) ──────────────────────────────────

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
            if (_ui.value.busyShareId != null) {
                _ui.update { it.copy(message = "Идёт передача, попробуйте позже") }
                return@launch
            }
            _ui.update {
                it.copy(
                    storageDialogFor = null, storagePromptMode = false,
                    busyShareId = shareId, progress = preparing(),
                )
            }
            val poll = launchPolling()
            val outcome = withContext(Dispatchers.IO) { repo.migrateStorage(cfg, newDir) }
            poll.cancel()
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
                    busyShareId = null, progress = null, localPaths = local,
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
            is Pending.Download -> store().get(p.shareId)?.let { downloadAndOpen(it, p.entry) }
            is Pending.Sync -> store().get(p.shareId)?.let { syncNow(it) }
            is Pending.EnableSync -> setSyncEnabled(p.shareId, true)
        }
    }

    private fun Pending.shareIdOf(): String = when (this) {
        is Pending.Download -> shareId
        is Pending.Sync -> shareId
        is Pending.EnableSync -> shareId
    }

    private fun migrateMessage(o: ShareRepository.MigrateOutcome, newPath: String?): String = when {
        o.error == "busy" -> "Идёт синхронизация, попробуйте позже"
        o.error?.startsWith("no_space") == true -> "Недостаточно места в новой папке"
        o.error != null -> "Не удалось перенести: ${o.error}"
        o.cancelled -> "Перенос отменён, папка не изменена"
        else -> "Папка: ${StorageAccess.label(newPath)} (перенесено ${o.moved})" +
            (if (o.conflicts > 0) ", конфликтов ${o.conflicts}" else "") +
            (if (o.failed > 0) ", ошибок ${o.failed}" else "")
    }

    private fun preparing() = Progress("Подготовка…", 0, 0, 0, 0, 0)

    /**
     * Poll native transfer progress until the launching operation cancels this
     * job. It does NOT stop on `active == false`: at the start the native side is
     * still fetching the manifest (not yet active), so breaking there would hide
     * the bar until a second tap. Speed is computed from the byte delta.
     */
    private fun launchPolling(): Job = viewModelScope.launch {
        var lastBytes = 0L
        var lastTime = System.currentTimeMillis()
        while (true) {
            val snap = withContext(Dispatchers.IO) { NativeBridge.nativeTransferProgress() }
            runCatching { JSONObject(snap) }.getOrNull()?.let { o ->
                // Before the native transfer flips active (manifest still
                // loading) the counters are zero/stale, so show a "preparing"
                // bar at 0 rather than leftover bytes from a previous transfer.
                val active = o.optBoolean("active", false)
                val bytesDone = if (active) o.optLong("bytes_done") else 0L
                val now = System.currentTimeMillis()
                val dt = (now - lastTime).coerceAtLeast(1)
                val speed = if (active) ((bytesDone - lastBytes) * 1000 / dt).coerceAtLeast(0) else 0
                lastBytes = bytesDone
                lastTime = now
                _ui.update {
                    it.copy(
                        progress = Progress(
                            file = if (active) o.optString("file").ifEmpty { "Подготовка…" } else "Подготовка…",
                            filesDone = if (active) o.optLong("files_done") else 0,
                            filesTotal = if (active) o.optLong("files_total") else 0,
                            bytesDone = bytesDone,
                            bytesTotal = if (active) o.optLong("bytes_total") else 0,
                            speedBytesPerSec = speed,
                        )
                    )
                }
            }
            delay(350)
        }
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
}
