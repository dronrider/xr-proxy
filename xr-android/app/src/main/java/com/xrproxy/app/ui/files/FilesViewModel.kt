package com.xrproxy.app.ui.files

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.data.ShareRepository
import com.xrproxy.app.data.ShareStore
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareGrant
import com.xrproxy.app.service.ShareSyncScheduler
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
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

    private val store = ShareStore.create(app)
    private val repo = ShareRepository(app)

    val configs: StateFlow<List<ShareConfig>> = store.shares

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
        val openShareId: String? = null,
        val currentPath: String = "",
        val manifest: List<ManifestEntry> = emptyList(),
        val manifestLoading: Boolean = false,
        val localPaths: Set<String> = emptySet(),
        val busyShareId: String? = null,
        val progress: Progress? = null,
        val openFileEvent: File? = null,
        val message: String? = null,
    )

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    fun consumeMessage() = _ui.update { it.copy(message = null) }
    fun consumeOpenEvent() = _ui.update { it.copy(openFileEvent = null) }

    // ── share list ──────────────────────────────────────────────────

    fun refreshHub(hubUrl: String?, inviteToken: String?) {
        if (hubUrl.isNullOrBlank() || inviteToken.isNullOrBlank()) {
            _ui.update { it.copy(message = "Нет инвайта — добавьте сервер по инвайту") }
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
                    onSuccess = { st.copy(hubShares = it, loadingHub = false) },
                    onFailure = { st.copy(loadingHub = false, message = "Шары по инвайту: ${it.message}") },
                )
            }
        }
    }

    /**
     * Refresh of the invite carries the agent's current address/port/token. If a
     * share was added earlier and the agent has since moved (e.g. a private LAN
     * address replaced by the public IP), update the stored connection fields in
     * place so we stop hitting the stale address. The user's selection and sync
     * toggle are kept; a remove + re-add is no longer needed.
     */
    private fun reconcileShares(grants: List<ShareGrant>) {
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
        store.upsert(ShareConfig.fromGrant(grant))
        _ui.update { it.copy(message = "Шара «${grant.name}» добавлена") }
    }

    fun removeShare(shareId: String) {
        store.remove(shareId)
        rescheduleIfNeeded()
        if (_ui.value.openShareId == shareId) closeShare()
    }

    // ── explorer ────────────────────────────────────────────────────

    fun openShare(config: ShareConfig) {
        _ui.update {
            it.copy(
                openShareId = config.shareId, currentPath = "",
                manifest = emptyList(), manifestLoading = true,
            )
        }
        viewModelScope.launch {
            val (result, local) = withContext(Dispatchers.IO) {
                repo.fetchManifest(config) to repo.localPaths(config.shareId)
            }
            _ui.update { st ->
                if (st.openShareId != config.shareId) return@update st
                result.fold(
                    onSuccess = { st.copy(manifest = it, manifestLoading = false, localPaths = local) },
                    onFailure = { st.copy(manifestLoading = false, message = "Манифест: ${it.message}") },
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
        store.update(shareId) { cfg ->
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
        setSelected(config.shareId, entry.path, true)
        localFile(config.shareId, entry.path)?.let {
            _ui.update { st -> st.copy(openFileEvent = it) }
            return
        }
        if (_ui.value.busyShareId != null) return
        _ui.update { it.copy(busyShareId = config.shareId, progress = preparing()) }
        val poll = launchPolling()
        viewModelScope.launch {
            val err = withContext(Dispatchers.IO) { repo.downloadOne(config, entry) }
            poll.cancel()
            val local = withContext(Dispatchers.IO) { repo.localPaths(config.shareId) }
            _ui.update {
                it.copy(
                    busyShareId = null, localPaths = local, progress = null,
                    openFileEvent = if (err == null) repo.fileFor(config.shareId, entry.path) else null,
                    message = when {
                        err == null -> null
                        err == "busy" -> "Идёт синхронизация, попробуй ещё раз"
                        else -> "Не удалось скачать"
                    },
                )
            }
        }
    }

    fun localFile(shareId: String, relPath: String): File? =
        repo.fileFor(shareId, relPath).takeIf { it.isFile }

    // ── sync + transfer control ─────────────────────────────────────

    fun setSyncEnabled(shareId: String, enabled: Boolean) {
        val cfg = store.get(shareId) ?: return
        if (enabled && !cfg.hasToken) {
            _ui.update { it.copy(message = "Нет токена доступа") }
            return
        }
        store.update(shareId) { it.copy(syncEnabled = enabled) }
        rescheduleIfNeeded()
        if (enabled) {
            ShareSyncScheduler.syncNow(getApplication())
            _ui.update { it.copy(message = "Синк включён (зеркалит выбранное, удаляет лишнее)") }
        }
    }

    fun syncNow(config: ShareConfig) {
        if (config.selection.isEmpty()) {
            _ui.update { it.copy(message = "Отметь галочками файлы или папки для синка") }
            return
        }
        if (_ui.value.busyShareId != null) return
        _ui.update { it.copy(busyShareId = config.shareId, progress = preparing()) }
        val poll = launchPolling()
        viewModelScope.launch {
            val outcome = withContext(Dispatchers.IO) { repo.syncOnce(config) }
            poll.cancel()
            val local = withContext(Dispatchers.IO) { repo.localPaths(config.shareId) }
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

    private fun rescheduleIfNeeded() {
        if (store.enabledShares().isNotEmpty()) ShareSyncScheduler.schedulePeriodic(getApplication())
        else ShareSyncScheduler.cancelPeriodic(getApplication())
    }

    fun syncAllNow() {
        if (store.enabledShares().isNotEmpty()) ShareSyncScheduler.syncNow(getApplication())
    }
}
