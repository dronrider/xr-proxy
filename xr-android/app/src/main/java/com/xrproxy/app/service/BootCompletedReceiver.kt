package com.xrproxy.app.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import com.xrproxy.app.data.WantedSessionRepository

/**
 * Встречает загрузку телефона и обновление приложения (XR-279): пока
 * сохранено желание быть подключённым, туннель возвращается сам, без
 * открытия приложения. Оба события это смерть процесса, после которой
 * возвращать туннель было некому.
 *
 * Само решение о восстановлении сервис не пересказывает: ресивер только
 * проверяет сохранённое желание и стартует сервис командой ACTION_RESTORE,
 * а дальше действует RestorePolicy вместе с сетевыми колбэками сервиса
 * (нет сети после перезагрузки это повтор по расписанию, а не провал).
 *
 * Force-stop со стороны пользователя не доставляет броадкасты вовсе, так что
 * «убить приложение» из настроек остаётся честным способом выключить всё.
 */
class BootCompletedReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        val source = when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED -> "загрузки телефона"
            Intent.ACTION_MY_PACKAGE_REPLACED -> "обновления приложения"
            else -> return
        }
        val prefs = context.getSharedPreferences("xr_proxy", Context.MODE_PRIVATE)
        val snap = WantedSessionRepository(prefs).snapshot()
        if (restoreConfigJson(snap.active, snap.configJson) == null) return
        context.startForegroundService(
            Intent(context, XrVpnService::class.java).apply {
                action = XrVpnService.ACTION_RESTORE
                putExtra(XrVpnService.EXTRA_RESTORE_SOURCE, source)
            },
        )
    }
}
