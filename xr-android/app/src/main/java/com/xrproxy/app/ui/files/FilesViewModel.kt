package com.xrproxy.app.ui.files

import android.app.Application
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xrproxy.app.data.ShareRepository
import com.xrproxy.app.data.ShareStore
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareInfo
import com.xrproxy.app.service.ShareSyncScheduler
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Drives the Files screen (LLD-19). Owns the configured-share store and the
 * repository; all blocking native/SAF work runs on [Dispatchers.IO]. Mirror /
 * diff / download logic lives in Rust — this only sequences it and tracks UI
 * state.
 */
class FilesViewModel(app: Application) : AndroidViewModel(app) {

    private val store = ShareStore.create(app)
    private val repo = ShareRepository(app)

    val configs: StateFlow<List<ShareConfig>> = store.shares

    data class UiState(
        val hubShares: List<ShareInfo> = emptyList(),
        val loadingHub: Boolean = false,
        val busyShareId: String? = null,
        val openShareId: String? = null,
        val manifest: List<ManifestEntry> = emptyList(),
        val manifestLoading: Boolean = false,
        val message: String? = null,
    )

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    fun consumeMessage() = _ui.update { it.copy(message = null) }

    /** Pull the hub's share index so the user can add shares. */
    fun refreshHub(hubUrl: String?) {
        if (hubUrl.isNullOrBlank()) {
            _ui.update { it.copy(message = "Хаб не настроен — добавьте сервер с hub_url") }
            return
        }
        _ui.update { it.copy(loadingHub = true) }
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) { repo.listShares(hubUrl) }
            _ui.update { st ->
                result.fold(
                    onSuccess = { st.copy(hubShares = it, loadingHub = false) },
                    onFailure = { st.copy(loadingHub = false, message = "Список шар: ${it.message}") },
                )
            }
        }
    }

    fun addShare(info: ShareInfo) {
        store.upsert(ShareConfig.fromInfo(info))
        _ui.update { it.copy(message = "Шара «${info.name}» добавлена — вставьте токен") }
    }

    fun removeShare(shareId: String) {
        store.remove(shareId)
        rescheduleIfNeeded()
        if (_ui.value.openShareId == shareId) {
            _ui.update { it.copy(openShareId = null, manifest = emptyList()) }
        }
    }

    fun setToken(shareId: String, tokenJson: String) {
        store.update(shareId) { it.copy(tokenJson = tokenJson.trim()) }
        _ui.update { it.copy(message = "Токен сохранён") }
    }

    /** Persist the SAF folder choice (caller already took persistable permission). */
    fun setFolder(shareId: String, treeUri: Uri) {
        store.update(shareId) { it.copy(treeUri = treeUri.toString()) }
        _ui.update { it.copy(message = "Папка выбрана") }
    }

    /** Browse a share's files (one-time download picker). */
    fun openShare(config: ShareConfig) {
        _ui.update { it.copy(openShareId = config.shareId, manifest = emptyList(), manifestLoading = true) }
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) { repo.fetchManifest(config) }
            _ui.update { st ->
                result.fold(
                    onSuccess = { st.copy(manifest = it, manifestLoading = false) },
                    onFailure = { st.copy(manifestLoading = false, message = "Манифест: ${it.message}") },
                )
            }
        }
    }

    fun closeShare() = _ui.update { it.copy(openShareId = null, manifest = emptyList()) }

    /** One-time download of [entries] into [treeUri]. */
    fun downloadSelected(config: ShareConfig, entries: List<ManifestEntry>, treeUri: Uri) {
        if (entries.isEmpty()) return
        _ui.update { it.copy(busyShareId = config.shareId) }
        viewModelScope.launch {
            val failed = withContext(Dispatchers.IO) { repo.downloadInto(config, entries, treeUri) }
            _ui.update {
                it.copy(
                    busyShareId = null,
                    message = if (failed.isEmpty()) "Скачано: ${entries.size}"
                    else "Скачано ${entries.size - failed.size}/${entries.size}, ошибок: ${failed.size}",
                )
            }
        }
    }

    /**
     * Turn background mirror on/off. Enabling requires a token and a folder;
     * the user is warned that mirror deletes locally what's gone on the server.
     */
    fun setSyncEnabled(shareId: String, enabled: Boolean) {
        val cfg = store.get(shareId) ?: return
        if (enabled && (!cfg.hasToken || cfg.treeUri == null)) {
            _ui.update { it.copy(message = "Для синка нужны токен и папка") }
            return
        }
        store.update(shareId) { it.copy(syncEnabled = enabled) }
        rescheduleIfNeeded()
        if (enabled) {
            ShareSyncScheduler.syncNow(getApplication())
            _ui.update { it.copy(message = "Синк включён (зеркало: удаляет локально пропавшее на сервере)") }
        }
    }

    /** Run a mirror cycle for one share immediately. */
    fun syncNow(config: ShareConfig) {
        _ui.update { it.copy(busyShareId = config.shareId) }
        viewModelScope.launch {
            val outcome = withContext(Dispatchers.IO) { repo.syncOnce(config) }
            _ui.update {
                it.copy(
                    busyShareId = null,
                    message = if (outcome.ok)
                        "Синк «${config.name}»: +${outcome.fetched.size} −${outcome.deleted.size}"
                    else "Синк «${config.name}»: ${outcome.error}",
                )
            }
        }
    }

    /** Schedule or cancel the periodic worker based on whether any share is enabled. */
    private fun rescheduleIfNeeded() {
        if (store.enabledShares().isNotEmpty()) ShareSyncScheduler.schedulePeriodic(getApplication())
        else ShareSyncScheduler.cancelPeriodic(getApplication())
    }

    /** Sync all enabled shares now — called when the screen comes to foreground. */
    fun syncAllNow() {
        if (store.enabledShares().isNotEmpty()) ShareSyncScheduler.syncNow(getApplication())
    }
}
