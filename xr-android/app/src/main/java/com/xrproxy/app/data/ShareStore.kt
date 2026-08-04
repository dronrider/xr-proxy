package com.xrproxy.app.data

import android.content.Context
import android.content.SharedPreferences
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import com.xrproxy.app.model.FileSort
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.SortOrder
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONArray
import org.json.JSONObject

/**
 * Persistence for configured shares (LLD-19). Holds, per share, the access
 * token (handed out-of-band) and the chosen SAF tree — both sensitive, so this
 * is backed by [EncryptedSharedPreferences]. Exposes the list as a [StateFlow]
 * for the UI, mirroring [ServerRepository].
 *
 * Здесь же живут привычки проводника (XR-251): порядок строк и отметки
 * просмотренных файлов. Имена файлов чужой шары не менее чувствительны, чем
 * её токен, и второго Keystore-хранилища ради двух ключей заводить незачем.
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

    /** Убирает шару вместе с её отметками просмотра: иначе они копились бы в
     *  хранилище от каждой снятой шары и ротации инвайтов. Придержать их на
     *  время undo-снекбара это дело вызывающего ([viewed] до удаления,
     *  [setViewed] на возврат). */
    fun remove(shareId: String) {
        setViewed(shareId, emptySet())
        persist(_shares.value.filterNot { it.shareId == shareId })
    }

    /** Shares with background mirror enabled and a usable token. */
    fun enabledShares(): List<ShareConfig> =
        _shares.value.filter { it.syncEnabled && it.hasToken }

    // -- Проводник (XR-251) ------------------------------------------
    //
    // Зовётся всё это с главного потока, как и остальные методы класса: отметки
    // просмотра лежат одним JSON-блобом на все шары, и read-modify-write с пула
    // потоков терял бы отметку, когда два файла открывают подряд.

    /** Порядок строк проводника, общий на все шары. */
    fun sortOrder(): SortOrder {
        val mode = runCatching { FileSort.valueOf(prefs.getString(KEY_SORT, "").orEmpty()) }
            .getOrDefault(FileSort.NAME)
        return SortOrder(mode, prefs.getBoolean(KEY_SORT_DESC, SortOrder.of(mode).descending))
    }

    fun setSortOrder(order: SortOrder) {
        prefs.edit()
            .putString(KEY_SORT, order.mode.name)
            .putBoolean(KEY_SORT_DESC, order.descending)
            .apply()
    }

    /** Фильтр «только непросмотренные» (XR-256), общий на все шары. */
    fun unviewedOnly(): Boolean = prefs.getBoolean(KEY_UNVIEWED_ONLY, false)

    fun setUnviewedOnly(on: Boolean) {
        prefs.edit().putBoolean(KEY_UNVIEWED_ONLY, on).apply()
    }

    /** Пути шары, которые с этого устройства уже открывали. Отметка живёт по
     *  паре (шара, путь) и держится здесь, а не в манифесте: манифест приходит
     *  от агента и знать про наши просмотры не может, а удаление локальной
     *  копии просмотр не отменяет. */
    fun viewed(shareId: String): Set<String> {
        val arr = readViewed().optJSONArray(shareId) ?: return emptySet()
        return (0 until arr.length()).mapNotNull { arr.optString(it).takeIf { s -> s.isNotEmpty() } }.toSet()
    }

    fun markViewed(shareId: String, path: String) {
        val all = readViewed()
        val arr = all.optJSONArray(shareId) ?: JSONArray()
        for (i in 0 until arr.length()) if (arr.optString(i) == path) return
        arr.put(path)
        all.put(shareId, arr)
        prefs.edit().putString(KEY_VIEWED, all.toString()).apply()
    }

    /** Переписать отметки шары целиком: пустой набор их снимает (удаление
     *  шары), непустой возвращает придержанные (отмена удаления). */
    fun setViewed(shareId: String, paths: Set<String>) {
        val all = readViewed()
        if (paths.isEmpty()) all.remove(shareId)
        else all.put(shareId, JSONArray().apply { paths.forEach { put(it) } })
        prefs.edit().putString(KEY_VIEWED, all.toString()).apply()
    }

    private fun readViewed(): JSONObject =
        runCatching { JSONObject(prefs.getString(KEY_VIEWED, "").orEmpty()) }.getOrDefault(JSONObject())

    companion object {
        private const val KEY = "shares_v1"
        private const val KEY_SORT = "explorer_sort"
        private const val KEY_SORT_DESC = "explorer_sort_desc"
        private const val KEY_VIEWED = "explorer_viewed"
        private const val KEY_UNVIEWED_ONLY = "explorer_unviewed_only"

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
