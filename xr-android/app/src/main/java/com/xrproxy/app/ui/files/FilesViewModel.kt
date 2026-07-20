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
        val localPaths: Set<String> = emptySet(),
        /** FIFO of files queued with the per-row plus (XR-044); the head is the
         *  one being downloaded (or waiting out the background mirror's lock). */
        val queue: List<QueueItem> = emptyList(),
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
        /** Share whose storage-directory dialog is open (XR-043), or null. */
        val storageDialogFor: String? = null,
        /** True when the dialog is the first-sync prompt (auto-continues the
         *  deferred action on choice) vs. a later "change folder" from settings. */
        val storagePromptMode: Boolean = false,
        /** Share whose "Импорт по URL" dialog is open (LLD-29), or null. */
        val importDialogFor: String? = null,
        /** Live import job: the agent downloads, this screen polls every 2
         *  seconds. Leaving the screen stops the poll, not the download. */
        val importJob: ImportJob? = null,
    )

    /** A URL-import job the open screen is tracking (LLD-29). */
    data class ImportJob(
        val shareId: String,
        val jobId: String,
        /** Percent from the agent; null until the plugin reports any. */
        val progress: Double? = null,
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

    init {
        viewModelScope.launch {
            val store = store()
            _ui.update { it.copy(storeReady = true) }
            store.shares.collect { _configs.value = it }
        }
    }

    fun consumeMessage() = _ui.update { it.copy(message = null) }
    fun consumeOpenEvent() = _ui.update { it.copy(openFileEvent = null) }

    // -- share list --------------------------------------------------

    fun refreshHub(hubUrl: String?, inviteToken: String?) {
        this.hubUrl = hubUrl
        this.inviteToken = inviteToken
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

    /** Native error strings are category-prefixed and carry the human wording
     *  after the prefix; show that instead of the machine category, keeping the
     *  text's single source in Rust (XR-134). A stale token has no per-error
     *  detail worth showing, so it gets a fixed line (XR-167). */
    private fun humanError(e: String): String = when {
        e.startsWith("agent_offline") -> e.substringAfter(": ", "агент шары не на связи")
        e.startsWith("access_expired") -> e.substringAfter(": ", "доступ к шаре истёк")
        e.startsWith("stale_token") -> "токен шары устарел, обновите список по инвайту"
        else -> e
    }

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
                existing.agentPubkey != g.agentPubkey || existing.tokenJson != g.tokenJson ||
                existing.relayJson != g.relayJson
            ) {
                store.update(g.shareId) {
                    it.copy(
                        addr = g.addr, port = g.port, agentPubkey = g.agentPubkey,
                        tokenJson = g.tokenJson, name = g.name,
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
            _ui.update { it.copy(message = "Шара «${grant.name}» добавлена") }
        }
    }

    fun removeShare(shareId: String) {
        viewModelScope.launch {
            store().remove(shareId)
            rescheduleIfNeeded()
            _ui.update { st ->
                st.copy(
                    queue = st.queue.filterNot { it.shareId == shareId },
                    failed = st.failed.filterNot { it.shareId == shareId },
                )
            }
            // A live transfer of the removed share (our head or its mirror
            // pass) would otherwise write into the dead directory for up to an
            // hour and hold the single-transfer lock the whole time.
            freshSnapshot()?.let { o ->
                if (o.optString("share") == shareId) {
                    withContext(Dispatchers.IO) { NativeBridge.nativeCancelTransfer() }
                }
            }
            if (_ui.value.openShareId == shareId) closeShare()
        }
    }

    // -- explorer ----------------------------------------------------

    fun openShare(config: ShareConfig) {
        _ui.update {
            it.copy(
                openShareId = config.shareId, currentPath = "",
                manifest = emptyList(), manifestLoading = true, offlineLocal = false,
            )
        }
        ensureTransferPolling()
        viewModelScope.launch {
            // Cache-first (XR-059): the already-downloaded files show up right
            // away, the fresh manifest replaces them when the fetch lands. The
            // old fetch-then-fallback order kept a spinner up for the whole
            // manifest timeout when the VPN is up but the server unreachable:
            // the local smoltcp stack accepts the connect (SYN-ACK before the
            // upstream), so the connect-timeout never fires.
            val localManifest = withContext(Dispatchers.IO) { repo.localManifest(config) }
            val local = localManifest.map { it.path }.toSet()
            adoptLocalIntoSelection(config.shareId, local)
            if (localManifest.isNotEmpty()) {
                _ui.update { st ->
                    if (st.openShareId != config.shareId) return@update st
                    st.copy(manifest = localManifest, manifestLoading = false, localPaths = local, offlineLocal = true)
                }
            }
            val result = fetchManifestHealing(config)
            _ui.update { st ->
                if (st.openShareId != config.shareId) return@update st
                result.fold(
                    onSuccess = {
                        st.copy(
                            manifest = withLocalOnly(it, localManifest),
                            manifestLoading = false, localPaths = local, offlineLocal = false,
                        )
                    },
                    onFailure = { e ->
                        when {
                            // The agent is gone from the relay: mark the share
                            // offline and say so, instead of the raw network
                            // error against the loopback address (XR-134).
                            e.isAgentOffline() ->
                                st.copy(
                                    manifestLoading = false, offlineLocal = true,
                                    message = humanError(e.message.orEmpty())
                                        .replaceFirstChar { c -> c.uppercaseChar() },
                                )
                            // The stored token was pre-scope and the grant refresh
                            // proved the access is gone: say so plainly, keep any
                            // downloaded files viewable, never show serde (XR-167).
                            e.isAccessExpired() ->
                                st.copy(
                                    manifestLoading = false, offlineLocal = true,
                                    message = humanError(e.message.orEmpty()),
                                )
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
            if (result.isSuccess) enqueueMissingSelected(config.shareId)
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
                            manifestLoading = false, offlineLocal = false,
                        )
                    },
                    onFailure = { e ->
                        st.copy(
                            manifestLoading = false,
                            // No network (also the offline outcome of a stale-token
                            // heal, XR-167): a clean line, not the raw reqwest text.
                            message = if (e.isOffline()) "Список: хаб недоступен, попробуйте позже"
                            else "Список: ${humanError(e.message ?: "ошибка")}",
                        )
                    },
                )
            }
            if (result.isSuccess) enqueueMissingSelected(config.shareId)
        }
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
            s.copy(
                queue = s.queue + missing.map { QueueItem(shareId, it) },
                failed = s.failed.filterNot { it.shareId == shareId && it.path in missingPaths },
            )
        }
        ensureQueueRunning()
    }

    fun closeShare() {
        // The poll lives as long as the share screen; the agent-side download
        // continues without us and the file shows up on the next open.
        stopImportPolling()
        _ui.update {
            it.copy(
                openShareId = null, currentPath = "", manifest = emptyList(),
                localPaths = emptySet(), importJob = null, importDialogFor = null,
            )
        }
    }

    fun navigateTo(path: String) = _ui.update { it.copy(currentPath = path) }

    fun navigateUp() {
        val p = _ui.value.currentPath
        if (p.isEmpty()) closeShare()
        else _ui.update { it.copy(currentPath = p.substringBeforeLast('/', "")) }
    }

    /** Tap a downloaded row: hand the local file to a viewer app. A file that
     *  vanished from disk (deleted via a file manager) flips its row back to
     *  the plus instead of ignoring the tap. */
    fun openLocal(config: ShareConfig, entry: ManifestEntry) {
        viewModelScope.launch {
            val existing = withContext(Dispatchers.IO) { localFile(config, entry.path) }
            if (existing != null) {
                _ui.update { it.copy(openFileEvent = existing) }
            } else {
                _ui.update { st ->
                    st.copy(
                        localPaths = if (st.openShareId == config.shareId) st.localPaths - entry.path else st.localPaths,
                        message = "Файла уже нет на устройстве",
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
                else st.copy(
                    queue = st.queue + QueueItem(config.shareId, entry),
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
                s.copy(
                    queue = s.queue + missing.map { QueueItem(config.shareId, it) },
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
                    withContext(Dispatchers.IO) { NativeBridge.nativeCancelTransfer() }
                }
            }
            awaitWriterLeft(config.shareId, prefix)
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
                    message = if (stillWanted) "Офлайн: папка остаётся выбранной и вернётся при синке" else st.message,
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
                    message = if (stillWanted) "Офлайн: файл остаётся выбранным и вернётся при синке" else st.message,
                )
            }
        }
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
                withContext(Dispatchers.IO) { NativeBridge.nativeCancelTransfer() }
            }
        }
    }

    /** Bounded wait until the native writer can no longer touch [prefix]: not
     *  inside it and not a multi-file pass of this share (a cancelled download
     *  flushes its partial at the next chunk, so this is quick). */
    private suspend fun awaitWriterLeft(shareId: String, prefix: String) = withContext(Dispatchers.IO) {
        repeat(30) {
            val o = runCatching { JSONObject(NativeBridge.nativeTransferProgress()) }.getOrNull()
                ?: return@withContext
            if (!o.optBoolean("active") || o.optString("share") != shareId ||
                (!o.optString("file").startsWith(prefix) && o.optLong("files_total") <= 1)
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
                    st.copy(
                        queue = st.queue.filter { it !== item },
                        localPaths = if (done && st.openShareId == item.shareId) st.localPaths + item.entry.path
                        else st.localPaths,
                        failed = if (done || cancelled) st.failed
                        else st.failed + FailedDownload(
                            item.shareId, item.entry.path,
                            bytesDone, item.entry.size, humanError(err ?: "ошибка"),
                        ),
                        // One toast per burst: the first failure says why, the
                        // rest just turn their rows red (a dead network would
                        // otherwise queue a toast per file).
                        message = if (!done && !cancelled && st.failed.isEmpty()) {
                            "Не скачался «${item.entry.path.substringAfterLast('/')}»: ${humanError(err ?: "ошибка")}"
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

    /**
     * Poll the native transfer snapshot while the explorer is open, the queue
     * is busy or a migration runs. One poller covers the foreground queue, the
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
            while (_ui.value.openShareId != null || _ui.value.queue.isNotEmpty() ||
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
                _ui.update { it.copy(message = "Фоновый синк включён: докачивает выбранное и убирает удалённое на сервере") }
            }
        }
    }

    /** Cancel the running native transfer (the storage-migration card's stop). */
    fun cancelTransfer() {
        viewModelScope.launch { withContext(Dispatchers.IO) { NativeBridge.nativeCancelTransfer() } }
        _ui.update { it.copy(message = "Останавливаю...") }
    }

    // -- URL import (LLD-29) ----------------------------------------

    private var importPoll: Job? = null

    /** Consecutive failed polls tolerated before giving up: one lost poll on a
     *  network handover must not orphan a job that keeps running on the agent. */
    private val importPollFailureLimit = 3

    fun openImportDialog(shareId: String) = _ui.update { it.copy(importDialogFor = shareId) }
    fun dismissImportDialog() = _ui.update { it.copy(importDialogFor = null) }

    /** Start importing [url] into the currently open folder of the share.
     *  [height] null means "Максимум": the owner's cap alone limits quality. */
    fun startImport(config: ShareConfig, url: String, height: Int?) {
        // One tracked job at a time: a second start would orphan the first
        // (untrackable, uncancellable) on the agent's single-worker queue.
        if (_ui.value.importJob != null) {
            _ui.update { it.copy(importDialogFor = null, message = "Импорт уже идёт, дождись или отмени его") }
            return
        }
        val dest = _ui.value.currentPath
        _ui.update { it.copy(importDialogFor = null) }
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) {
                repo.importUrl(config, url.trim(), dest, height)
            }
            result.fold(
                onSuccess = { jobId ->
                    _ui.update { it.copy(importJob = ImportJob(config.shareId, jobId)) }
                    startImportPolling(config, jobId)
                },
                onFailure = { e ->
                    _ui.update {
                        it.copy(message = "Импорт: ${humanImportError(e.message ?: "ошибка")}")
                    }
                },
            )
        }
    }

    /** The cross on the import row: kill the download on the agent, forget the job. */
    fun cancelImport(config: ShareConfig) {
        val job = _ui.value.importJob ?: return
        stopImportPolling()
        _ui.update { it.copy(importJob = null) }
        viewModelScope.launch(Dispatchers.IO) { repo.importCancel(config, job.jobId) }
    }

    /** Poll every 2 seconds while the screen is open (LLD-29 п. 2.8): on done
     *  the row disappears and the listing refreshes, on failed the agent's
     *  error text is shown. Transient poll failures are tolerated up to
     *  [importPollFailureLimit] in a row: the job runs on the agent and one
     *  lost network round-trip says nothing about it. */
    private fun startImportPolling(config: ShareConfig, jobId: String) {
        stopImportPolling()
        importPoll = viewModelScope.launch {
            var failures = 0
            while (true) {
                delay(2_000)
                val result = withContext(Dispatchers.IO) { repo.importStatus(config, jobId) }
                val state = result.getOrNull()
                when {
                    state == null -> {
                        failures++
                        if (failures >= importPollFailureLimit) {
                            _ui.update {
                                it.copy(
                                    importJob = null,
                                    message = "Импорт: ${humanImportError(result.exceptionOrNull()?.message ?: "ошибка")}",
                                )
                            }
                            break
                        }
                    }
                    state.state == "done" -> {
                        _ui.update { it.copy(importJob = null, message = "Импорт завершён") }
                        refreshOpenShare(config)
                        break
                    }
                    state.state == "failed" -> {
                        _ui.update {
                            it.copy(
                                importJob = null,
                                message = "Импорт не удался: ${state.error ?: "причина неизвестна"}",
                            )
                        }
                        break
                    }
                    else -> {
                        failures = 0
                        _ui.update { st ->
                            st.copy(importJob = st.importJob?.copy(progress = state.progress))
                        }
                    }
                }
            }
        }
    }

    private fun stopImportPolling() {
        importPoll?.cancel()
        importPoll = null
    }

    /** Import errors carry a machine prefix (`no_plugin: ...`); the text after
     *  it is already human-worded and single-sourced in Rust. */
    private fun humanImportError(e: String): String =
        if (Regex("^[a-z_]+: ").containsMatchIn(e)) e.substringAfter(": ") else e

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
                _ui.update { it.copy(message = "Идёт передача, попробуйте позже") }
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
        o.error == "busy" -> "Идёт синхронизация, попробуйте позже"
        o.error?.startsWith("no_space") == true -> "Недостаточно места в новой папке"
        o.error != null -> "Не удалось перенести: ${o.error}"
        o.cancelled -> "Перенос отменён, папка не изменена"
        else -> "Папка: ${StorageAccess.label(newPath)} (перенесено ${o.moved})" +
            (if (o.conflicts > 0) ", конфликтов ${o.conflicts}" else "") +
            (if (o.failed > 0) ", ошибок ${o.failed}" else "")
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
         *  Category-prefixed like the native errors so [humanError] renders the
         *  wording after the colon and the folds route it apart from serde. */
        const val ERR_ACCESS_EXPIRED =
            "access_expired: Доступ к шаре истёк, удалите её или перевыпустите инвайт"

        /** Stale token, but the hub was unreachable to refresh the grant: reuse
         *  the "network:" category so the offline handling kicks in (XR-167). */
        const val ERR_HUB_OFFLINE = "network: хаб недоступен, обновите список позже"
    }
}
