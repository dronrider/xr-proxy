package com.xrproxy.app.ui.files

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.data.ShareRepository
import com.xrproxy.app.data.ShareStore
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareGrant
import com.xrproxy.app.service.ShareSyncScheduler
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File

/**
 * Drives the Files screen (LLD-19, XR-031): a list of shares ("drives"), and an
 * Explorer that navigates one share's folders. Files mirror into the app's own
 * directory; selection (files / folder prefixes) drives the background mirror.
 */
class FilesViewModel(app: Application) : AndroidViewModel(app) {

    private val store = ShareStore.create(app)
    private val repo = ShareRepository(app)

    val configs: StateFlow<List<ShareConfig>> = store.shares

    data class UiState(
        val hubShares: List<ShareGrant> = emptyList(),
        val loadingHub: Boolean = false,
        // Explorer
        val openShareId: String? = null,
        val currentPath: String = "",
        val manifest: List<ManifestEntry> = emptyList(),
        val manifestLoading: Boolean = false,
        val localPaths: Set<String> = emptySet(),
        val busyShareId: String? = null,
        val message: String? = null,
    )

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    fun consumeMessage() = _ui.update { it.copy(message = null) }

    // ── share list ──────────────────────────────────────────────────

    /** List the shares attached to this server's invite (the token rides along). */
    fun refreshHub(hubUrl: String?, inviteToken: String?) {
        if (hubUrl.isNullOrBlank() || inviteToken.isNullOrBlank()) {
            _ui.update { it.copy(message = "Нет инвайта — добавьте сервер по инвайту") }
            return
        }
        _ui.update { it.copy(loadingHub = true) }
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) { repo.inviteShares(hubUrl, inviteToken) }
            _ui.update { st ->
                result.fold(
                    onSuccess = { st.copy(hubShares = it, loadingHub = false) },
                    onFailure = { st.copy(loadingHub = false, message = "Шары по инвайту: ${it.message}") },
                )
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

    /** Enter a share: load its manifest + what is already downloaded. */
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

    /** Up one level; at the root, leave the share. */
    fun navigateUp() {
        val p = _ui.value.currentPath
        if (p.isEmpty()) closeShare()
        else _ui.update { it.copy(currentPath = p.substringBeforeLast('/', "")) }
    }

    /** Tick/untick a file or folder for sync. Selecting a folder subsumes (and
     *  clears) any individually-selected descendants; deselecting clears them. */
    fun setSelected(shareId: String, path: String, selected: Boolean) {
        store.update(shareId) { cfg ->
            val sel = cfg.selection.toMutableSet()
            sel.removeAll { it == path || it.startsWith("$path/") }
            if (selected) sel.add(path)
            cfg.copy(selection = sel)
        }
    }

    /** One-time download of a single file. */
    fun download(config: ShareConfig, entry: ManifestEntry) {
        _ui.update { it.copy(busyShareId = config.shareId) }
        viewModelScope.launch {
            val ok = withContext(Dispatchers.IO) { repo.downloadOne(config, entry) }
            val local = withContext(Dispatchers.IO) { repo.localPaths(config.shareId) }
            _ui.update {
                it.copy(
                    busyShareId = null, localPaths = local,
                    message = if (ok) "Скачано: ${entry.path.substringAfterLast('/')}" else "Не удалось скачать",
                )
            }
        }
    }

    /** Local file for a share-relative path, if it is downloaded (for opening). */
    fun localFile(shareId: String, relPath: String): File? =
        repo.fileFor(shareId, relPath).takeIf { it.isFile }

    // ── sync ────────────────────────────────────────────────────────

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
        _ui.update { it.copy(busyShareId = config.shareId) }
        viewModelScope.launch {
            val outcome = withContext(Dispatchers.IO) { repo.syncOnce(config) }
            val local = withContext(Dispatchers.IO) { repo.localPaths(config.shareId) }
            _ui.update {
                it.copy(
                    busyShareId = null, localPaths = local,
                    message = if (outcome.ok)
                        "Синк «${config.name}»: +${outcome.fetched} −${outcome.deleted}" +
                            if (outcome.failed > 0) " (ошибок ${outcome.failed})" else ""
                    else "Синк «${config.name}»: ${outcome.error}",
                )
            }
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
