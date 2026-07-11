package com.xrproxy.app

import android.app.Application
import android.content.Context
import com.xrproxy.app.data.JournalSettings
import com.xrproxy.app.data.UserRulesStore

/**
 * Роль Application-класса: поднять персистентный журнал (XR-042) раньше любых
 * его потребителей и провести разовые миграции. Application.onCreate
 * выполняется до любого компонента процесса (Activity, VPN-сервис,
 * WorkManager-воркер фонового синка), поэтому все записи с первых мгновений
 * попадают в файл, а миграция правил успевает прочитать легаси-поля профилей
 * до того, как их перезапишут без этих полей.
 */
class XrApp : Application() {
    override fun onCreate() {
        super.onCreate()
        val prefs = getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)
        JournalSettings.apply(prefs, filesDir)
        UserRulesStore.migrateIfNeeded(prefs, filesDir)
    }
}
