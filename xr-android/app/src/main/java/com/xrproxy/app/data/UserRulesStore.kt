package com.xrproxy.app.data

import android.content.SharedPreferences
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.UUID

/**
 * Одно пользовательское правило маршрутизации (LLD-05, XR-047). Список
 * глобальный: правила накладываются поверх пресета любого активного сервера
 * и срабатывают первыми (первое совпадение выигрывает). `id` нужен только
 * как ключ Compose-списка, в Rust не уезжает.
 */
data class UserRule(
    val id: String = UUID.randomUUID().toString(),
    val action: String,
    val pattern: String,
)

/**
 * Хранилище пользовательских правил: `filesDir/user_rules.json`, формат
 * `{"version":1,"rules":[{id,action,pattern}...]}`. Запись атомарная через
 * временный файл, чтобы смерть процесса на середине не оставила битый JSON.
 */
object UserRulesStore {

    private const val FILE_NAME = "user_rules.json"
    const val MAX_RULES = 100

    fun load(filesDir: File): List<UserRule> {
        val file = File(filesDir, FILE_NAME)
        if (!file.exists()) return emptyList()
        return try {
            val root = JSONObject(file.readText())
            val arr = root.optJSONArray("rules") ?: return emptyList()
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                val pattern = o.optString("pattern")
                if (pattern.isBlank()) return@mapNotNull null
                UserRule(
                    id = o.optString("id").ifBlank { UUID.randomUUID().toString() },
                    action = o.optString("action", "proxy"),
                    pattern = pattern,
                )
            }
        } catch (e: Exception) {
            Log.w("UserRules", "corrupted $FILE_NAME, starting empty: $e")
            emptyList()
        }
    }

    fun save(filesDir: File, rules: List<UserRule>) {
        val root = JSONObject().apply {
            put("version", 1)
            put("rules", JSONArray().apply {
                rules.forEach { r ->
                    put(JSONObject().apply {
                        put("id", r.id)
                        put("action", r.action)
                        put("pattern", r.pattern)
                    })
                }
            })
        }
        val file = File(filesDir, FILE_NAME)
        val tmp = File(filesDir, "$FILE_NAME.tmp")
        tmp.writeText(root.toString())
        if (!tmp.renameTo(file)) {
            // renameTo на одном томе не падает; если всё же упал, пишем прямо.
            file.writeText(root.toString())
            tmp.delete()
        }
    }

    /** Массив для конфига движка: `[{"action":..,"pattern":..}...]`, без id. */
    fun toConfigJson(rules: List<UserRule>): JSONArray = JSONArray().apply {
        rules.forEach { r ->
            put(JSONObject().apply {
                put("action", r.action)
                put("pattern", r.pattern)
            })
        }
    }

    // ── Миграция старых per-profile правил (LLD-05 §3.9) ─────────────

    private const val KEY_MIGRATED = "rules_migrated"

    /**
     * Разовая конвертация старой модели в глобальный список: кастомные
     * домены/CIDR каждого профиля становятся правилами `proxy`, профиль с
     * `proxy_all` даёт правило `*` -> proxy. Пресет Russia не переносится,
     * его роль у пресета хаба. Читает сырой JSON профилей из prefs (поля
     * `custom_domains`/`custom_ip_ranges` из модели уже выпилены) плюс
     * плоские prefs совсем старых установок. Вызывается из XrApp.onCreate,
     * до того как кто-либо перезапишет профили без легаси-полей.
     */
    fun migrateIfNeeded(prefs: SharedPreferences, filesDir: File) {
        if (prefs.getBoolean(KEY_MIGRATED, false)) return
        try {
            val patterns = LinkedHashSet<String>()
            var proxyAll = false

            fun collect(routingPreset: String, domains: String, ipRanges: String) {
                if (routingPreset == "proxy_all") proxyAll = true
                (domains.lines() + ipRanges.lines())
                    .map { it.trim().lowercase() }
                    .filter { it.isNotBlank() }
                    .forEach { patterns.add(it) }
            }

            prefs.getString("servers", null)?.let { raw ->
                val arr = JSONArray(raw)
                for (i in 0 until arr.length()) {
                    val p = arr.optJSONObject(i) ?: continue
                    collect(
                        p.optString("routing_preset"),
                        p.optString("custom_domains"),
                        p.optString("custom_ip_ranges"),
                    )
                }
            }
            // Совсем старые установки (до LLD-08): правила в плоских prefs.
            collect(
                prefs.getString("routing_preset", "") ?: "",
                prefs.getString("custom_domains", "") ?: "",
                prefs.getString("custom_ip_ranges", "") ?: "",
            )

            val migrated = buildList {
                patterns.forEach { add(UserRule(action = "proxy", pattern = it)) }
                if (proxyAll) add(UserRule(action = "proxy", pattern = "*"))
            }
            if (migrated.isNotEmpty() && load(filesDir).isEmpty()) {
                save(filesDir, migrated.take(MAX_RULES))
                Log.i("UserRules", "migrated ${migrated.size} legacy custom rule(s)")
            }
        } catch (e: Exception) {
            // Миграция не должна валить приложение: остаёмся с пустым списком.
            Log.w("UserRules", "legacy rules migration failed: $e")
        }
        prefs.edit().putBoolean(KEY_MIGRATED, true).apply()
    }
}
