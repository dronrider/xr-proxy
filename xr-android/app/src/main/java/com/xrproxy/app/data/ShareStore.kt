package com.xrproxy.app.data

import android.content.Context
import android.content.SharedPreferences
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import com.xrproxy.app.model.ShareConfig
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONArray

/**
 * Persistence for configured shares (LLD-19). Holds, per share, the access
 * token (handed out-of-band) and the chosen SAF tree — both sensitive, so this
 * is backed by [EncryptedSharedPreferences]. Exposes the list as a [StateFlow]
 * for the UI, mirroring [ServerRepository].
 */
class ShareStore(private val prefs: SharedPreferences) {

    private val _shares = MutableStateFlow<List<ShareConfig>>(emptyList())
    val shares: StateFlow<List<ShareConfig>> = _shares

    init {
        load()
    }

    private fun load() {
        val raw = prefs.getString(KEY, null)
        _shares.value = if (raw.isNullOrBlank()) emptyList() else runCatching {
            val arr = JSONArray(raw)
            (0 until arr.length()).map { ShareConfig.fromJson(arr.getJSONObject(it)) }
        }.getOrDefault(emptyList())
    }

    private fun persist(list: List<ShareConfig>) {
        val arr = JSONArray()
        list.forEach { arr.put(it.toJson()) }
        prefs.edit().putString(KEY, arr.toString()).apply()
        _shares.value = list
    }

    fun get(shareId: String): ShareConfig? = _shares.value.firstOrNull { it.shareId == shareId }

    /** Insert or replace by share id, preserving order. */
    fun upsert(config: ShareConfig) {
        val list = _shares.value.toMutableList()
        val idx = list.indexOfFirst { it.shareId == config.shareId }
        if (idx >= 0) list[idx] = config else list.add(config)
        persist(list)
    }

    fun update(shareId: String, transform: (ShareConfig) -> ShareConfig) {
        get(shareId)?.let { upsert(transform(it)) }
    }

    fun remove(shareId: String) {
        persist(_shares.value.filterNot { it.shareId == shareId })
    }

    /** Shares with background mirror enabled and a usable token. */
    fun enabledShares(): List<ShareConfig> =
        _shares.value.filter { it.syncEnabled && it.hasToken }

    companion object {
        private const val KEY = "shares_v1"

        fun create(context: Context): ShareStore {
            val masterKey = MasterKey.Builder(context)
                .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
                .build()
            val prefs = EncryptedSharedPreferences.create(
                context,
                "xr_shares",
                masterKey,
                EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
                EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
            )
            return ShareStore(prefs)
        }
    }
}
