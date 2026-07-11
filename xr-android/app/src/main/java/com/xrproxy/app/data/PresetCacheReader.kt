package com.xrproxy.app.data

import android.util.Log
import org.json.JSONObject
import java.io.File

/** Правило пресета в том виде, в котором его раздаёт хаб. */
data class CachedPresetRule(
    val action: String,
    val domains: List<String>,
    val ipRanges: List<String>,
    val geoip: List<String>,
)

/** Кэшированный пресет хаба для карточки и просмотра на экране правил. */
data class CachedPreset(
    val name: String,
    val version: Long,
    val updatedAt: String,
    val defaultAction: String,
    val rules: List<CachedPresetRule>,
)

/**
 * Читает дисковый кэш пресета (`filesDir/presets/<name>.json`), который
 * пишут Rust-сторона при apply инвайта, фоновом рефреше движка и кнопке
 * «Обновить сейчас». Kotlin кэш только читает, единственный писатель — Rust.
 */
object PresetCacheReader {

    fun read(cacheDir: File, presetName: String): CachedPreset? {
        val file = File(cacheDir, "$presetName.json")
        if (!file.exists()) return null
        return try {
            val root = JSONObject(file.readText())
            val rulesObj = root.optJSONObject("rules") ?: JSONObject()
            val rulesArr = rulesObj.optJSONArray("rules")
            val rules = (0 until (rulesArr?.length() ?: 0)).mapNotNull { i ->
                val r = rulesArr?.optJSONObject(i) ?: return@mapNotNull null
                CachedPresetRule(
                    action = r.optString("action", "proxy"),
                    domains = r.optJSONArray("domains").toStringList(),
                    ipRanges = r.optJSONArray("ip_ranges").toStringList(),
                    geoip = r.optJSONArray("geoip").toStringList(),
                )
            }
            CachedPreset(
                name = root.optString("name", presetName),
                version = root.optLong("version", 0),
                updatedAt = root.optString("updated_at", ""),
                defaultAction = rulesObj.optString("default_action", "direct"),
                rules = rules,
            )
        } catch (e: Exception) {
            Log.w("PresetCache", "unreadable preset cache ${file.name}: $e")
            null
        }
    }

    private fun org.json.JSONArray?.toStringList(): List<String> =
        if (this == null) emptyList()
        else (0 until length()).mapNotNull { optString(it).takeIf { s -> s.isNotBlank() } }
}
