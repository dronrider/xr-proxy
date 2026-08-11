package com.xrproxy.app.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * XR-088: до правки выбор другого сервера и применение инвайта под живым
 * туннелем упирались в снекбар «Сначала отключите VPN». Решение о том,
 * гасить ли туннель и трогать ли активный профиль, это чистая логика без
 * Android SDK, поэтому она вынесена в [ConnectionSwitch.kt] и проверяется
 * JVM-юнитом, а не эмулятором.
 */
class ConnectionSwitchTest {

    @Test
    fun selectingTheSameServerDoesNothing() {
        assertEquals(
            SwitchAction.Ignore,
            serverSelectAction(ConnectPhase.Connected, "a", "a"),
        )
    }

    @Test
    fun selectingWithoutTunnelJustSetsActive() {
        assertEquals(
            SwitchAction.SetActive,
            serverSelectAction(ConnectPhase.Idle, "a", "b"),
        )
    }

    @Test
    fun selectingUnderLiveTunnelReconnects() {
        // Раньше здесь был отказ с «Сначала отключите VPN».
        for (phase in listOf(
            ConnectPhase.Connected,
            ConnectPhase.Connecting,
            ConnectPhase.Preparing,
            ConnectPhase.Finalizing,
            ConnectPhase.Paused,
            ConnectPhase.Stopping,
        )) {
            assertEquals(
                phase.name,
                SwitchAction.Reconnect,
                serverSelectAction(phase, "a", "b"),
            )
        }
    }

    @Test
    fun firstProfileFromInviteBecomesActive() {
        assertTrue(inviteActivatesProfile(ConnectPhase.Idle, hasActiveProfile = false))
    }

    @Test
    fun inviteWithoutTunnelSwitchesToTheNewProfile() {
        assertTrue(inviteActivatesProfile(ConnectPhase.Idle, hasActiveProfile = true))
    }

    @Test
    fun inviteUnderLiveTunnelKeepsTheActiveProfile() {
        // Ключевой случай задачи: профиль добавляется, трафик не мигает.
        assertFalse(inviteActivatesProfile(ConnectPhase.Connected, hasActiveProfile = true))
        assertFalse(inviteActivatesProfile(ConnectPhase.Paused, hasActiveProfile = true))
    }
}
