package com.xrproxy.app.service

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

/**
 * Регресс XR-221: `onRevoke()` (системный отзыв VPN, включая перехват другим
 * клиентом) звал тот же путь, что и кнопка «Отключить», и в журнал ложилось
 * «туннель остановлен пользователем», хотя пользователь в приложении ничего
 * не нажимал. Само событие не проверить юнитом (нужен Android VpnService),
 * но текст двух причин остановки - чистая Kotlin-логика: фиксируем, что
 * причина «пользователь» упоминает именно пользователя, причина системного
 * отзыва его не упоминает, и тексты не совпадают.
 */
class VpnStopReasonTest {

    @Test
    fun uiReasonBlamesTheUser() {
        assertEquals("туннель остановлен пользователем", VpnStopReason.UI.journalMessage)
    }

    @Test
    fun systemRevokeReasonDoesNotClaimTheUserDidIt() {
        val message = VpnStopReason.SYSTEM_REVOKE.journalMessage
        assertNotEquals(VpnStopReason.UI.journalMessage, message)
        assert(!message.contains("пользователем")) {
            "системный отзыв не должен звучать как решение пользователя: $message"
        }
    }
}
