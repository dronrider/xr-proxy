package com.xrproxy.app.data

import com.xrproxy.app.jni.NativeBridge
import org.json.JSONObject
import java.io.File

/** Правило пресета в том виде, в котором его раздаёт хаб. */
data class CachedPresetRule(
    /** Название тематической группы, если хаб его прислал (XR-117). */
    val name: String,
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
 * Карточка пресета из дискового кэша. Сам кэш (`filesDir/presets/<name>.json`)
 * и пишет, и читает ядро: он его формат и завёл, а приложение раньше лезло
 * туда своим разбором и знало чужую структуру наизусть (XR-271). Теперь сюда
 * приезжает готовая сводка, и порт под другую платформу получает её тем же
 * вызовом.
 */
object PresetCacheReader {

    /** [trustedKey] это ключ проверки подписи из профиля сервера (XR-207):
     *  ядро сверяет им кэш, и карточка не показывает пресет, который движок
     *  применять откажется. Пустая строка значит «ключа нет». */
    fun read(cacheDir: File, presetName: String, trustedKey: String): CachedPreset? {
        val json = runCatching {
            NativeBridge.nativeCachedPreset(cacheDir.absolutePath, presetName, trustedKey)
        }.getOrNull() ?: return null
        return runCatching {
            val root = JSONObject(json)
            if (root.has("error")) return null
            val rulesArr = root.optJSONArray("rules")
            val rules = (0 until (rulesArr?.length() ?: 0)).mapNotNull { i ->
                val r = rulesArr?.optJSONObject(i) ?: return@mapNotNull null
                CachedPresetRule(
                    name = r.optString("name", ""),
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
                defaultAction = root.optString("default_action", "direct"),
                rules = rules,
            )
        }.getOrNull()
    }

    private fun org.json.JSONArray?.toStringList(): List<String> =
        if (this == null) emptyList()
        else (0 until length()).mapNotNull { optString(it).takeIf { s -> s.isNotBlank() } }
}
