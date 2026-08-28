package com.xrproxy.app.data

import android.content.SharedPreferences

/**
 * Желание пользователя быть подключённым, переживающее смерть процесса и
 * перезагрузку (XR-279). До него остановка туннеля любым событием вне
 * приложения (выгрузка процесса системой, ребут, свайп из недавних, самообнов
 * APK) оставляла флаг нигде: сервис перезапускался без конфига и гасился, а
 * экран показывал Idle, неотличимый от намеренного отключения.
 *
 * Хранится в том же `"xr_proxy"` SharedPreferences, что и остальные настройки,
 * читается свеже при каждом старте (сервис и ресивер держат свои экземпляры).
 *
 * Три составляющие:
 *  - [active]: пользователь включил туннель и не выключал; гасит только
 *    отключение из приложения (см. [com.xrproxy.app.service.clearsWantedOnStop]);
 *  - [configJson]: конфиг последнего пользовательского подключения целиком.
 *    Пресет и правила пользователя движок всё равно перечитывает на старте,
 *    так что восстановленная сессия свежая, а запасной путь «ключ профиля с
 *    пересборкой» не нужен и не пережил бы перезагрузку без сети;
 *  - [overrideSsid]: сеть, на которой пользователь сказал «включить здесь»,
 *    чтобы восстановление на ней не уронило туннель в авто-паузу.
 */
class WantedSessionRepository(private val prefs: SharedPreferences) {

    data class Snapshot(
        val active: Boolean,
        val configJson: String?,
        val overrideSsid: String?,
    )

    /** Пользователь подключился: желание активно, конфиг сессии сохранён. */
    fun startWanted(configJson: String) {
        prefs.edit()
            .putBoolean(KEY_ACTIVE, true)
            .putString(KEY_CONFIG, configJson)
            .apply()
    }

    /** Пользователь отключился сам: желание снято, сохранённое стирается. */
    fun stopWanted() {
        prefs.edit()
            .putBoolean(KEY_ACTIVE, false)
            .putString(KEY_CONFIG, null)
            .putString(KEY_OVERRIDE, null)
            .apply()
    }

    /** Обновить только override «включить здесь»: конфиг сессии не трогаем. */
    fun updateOverride(ssid: String?) {
        prefs.edit().putString(KEY_OVERRIDE, ssid).apply()
    }

    /** Свежий срез для решения о восстановлении. */
    fun snapshot(): Snapshot = Snapshot(
        active = prefs.getBoolean(KEY_ACTIVE, false),
        configJson = prefs.getString(KEY_CONFIG, null),
        overrideSsid = prefs.getString(KEY_OVERRIDE, null),
    )

    private companion object {
        const val KEY_ACTIVE = "wanted_session_active"
        const val KEY_CONFIG = "wanted_session_config"
        const val KEY_OVERRIDE = "wanted_session_override_ssid"
    }
}
