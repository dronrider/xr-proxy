package com.xrproxy.app.service

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Регресс XR-279: туннель не возвращался ни после убийства процесса, ни после
 * перезагрузки, ни после свайпа из недавних, потому что нигде не хранилось
 * желание пользователя быть подключённым. Само событие (смерть процесса,
 * BOOT_COMPLETED) юнитом не проверить, но решение «восстанавливать или нет»
 * по сохранённому состоянию, причине остановки и виду провала - чистая
 * Kotlin-логика: фиксируем каждую развилку таблицей.
 */
class RestorePolicyTest {

    // Восстанавливать ли по сохранённому состоянию

    @Test
    fun restoreUsesSavedConfigWhenUserWantsToStayConnected() {
        assertEquals("{\"servers\":[]}", restoreConfigJson(true, "{\"servers\":[]}"))
    }

    @Test
    fun noRestoreAfterExplicitDisconnect() {
        assertNull(restoreConfigJson(false, "{\"servers\":[]}"))
    }

    @Test
    fun noRestoreWithoutSavedConfig() {
        assertNull(restoreConfigJson(true, null))
        assertNull(restoreConfigJson(true, ""))
    }

    // Какая остановка гасит желание

    @Test
    fun onlyUserDisconnectClearsTheWantedFlag() {
        assertTrue(clearsWantedOnStop(VpnStopReason.UI))
        assertFalse(clearsWantedOnStop(VpnStopReason.SYSTEM_REVOKE))
    }

    // Что делать с неудачной попыткой

    @Test
    fun retryOnlyWhenTheEngineCouldNotReachTheServer() {
        assertFalse(shouldRetryRestore(RestoreFailureKind.TUN_ESTABLISH))
        assertFalse(shouldRetryRestore(RestoreFailureKind.ENGINE_START))
        assertTrue(shouldRetryRestore(RestoreFailureKind.ENGINE_DIED))
    }

    // Расписание повторов

    @Test
    fun retryDelayGrowsAndCapsAtOneMinute() {
        assertEquals(5_000L, restoreRetryDelayMs(0))
        assertEquals(10_000L, restoreRetryDelayMs(1))
        assertEquals(20_000L, restoreRetryDelayMs(2))
        assertEquals(40_000L, restoreRetryDelayMs(3))
        assertEquals(60_000L, restoreRetryDelayMs(4))
        assertEquals(60_000L, restoreRetryDelayMs(9))
    }

    @Test
    fun retryDelayNeverCollapsesToZeroOrNegative() {
        assertEquals(5_000L, restoreRetryDelayMs(-3))
        assertTrue(restoreRetryDelayMs(100) > 0)
    }

    // Повторный вход в восстановление

    @Test
    fun duplicateEntryIntoRestoreIsSkippedWhileWindowIsOpen() {
        // сервис занят живой сессией
        assertTrue(duplicateRestoreSkipped(phaseBusy = true, restorePending = false))
        // фаза ещё Idle, но окно восстановления уже открыто: второй старт
        // поднял бы TUN поверх первого
        assertTrue(duplicateRestoreSkipped(phaseBusy = false, restorePending = true))
        // свободно и окна нет: вход разрешён
        assertFalse(duplicateRestoreSkipped(phaseBusy = false, restorePending = false))
    }

    // Тип location у foreground-старта

    @Test
    fun locationTypeRequiresUiStartEvenWithPermission() {
        // фоновое восстановление после перезагрузки: разрешение есть, но
        // Android 14+ срезает while-in-use тип у фонового старта, сервис
        // остаётся без типа и establish() ломается
        assertFalse(foregroundLocationTypeAllowed(startedFromUi = false, locationGranted = true))
        assertTrue(foregroundLocationTypeAllowed(startedFromUi = true, locationGranted = true))
        // без разрешения типа быть не должно и у UI-старта: на Android 14+
        // старт с типом без разрешения бросает исключение
        assertFalse(foregroundLocationTypeAllowed(startedFromUi = true, locationGranted = false))
        assertFalse(foregroundLocationTypeAllowed(startedFromUi = false, locationGranted = false))
    }
}
