package com.xrproxy.app.ui

/**
 * Что сделать с подключением, когда пользователь выбирает другой сервер
 * (XR-088). Раньше выбор под живым туннелем упирался в снекбар «Сначала
 * отключите VPN»: активный профиль читается один раз, на старте движка, и
 * подмена его на ходу оставила бы карточку с одним сервером, а трафик в
 * другом. Гасить туннель руками ради этого незачем, приложение умеет
 * погасить и поднять само.
 */
enum class SwitchAction {
    /** Выбран тот же сервер, делать нечего. */
    Ignore,

    /** Туннеля нет: просто пометить активным. */
    SetActive,

    /** Туннель живой: пометить активным и переподнять его на новом профиле. */
    Reconnect,
}

/** Решение для «выбрать сервер». Чистая логика без Android SDK, поэтому
 *  живёт отдельным файлом и покрыта JVM-юнитом. */
fun serverSelectAction(
    phase: ConnectPhase,
    activeId: String?,
    targetId: String,
): SwitchAction = when {
    activeId == targetId -> SwitchAction.Ignore
    phase == ConnectPhase.Idle -> SwitchAction.SetActive
    else -> SwitchAction.Reconnect
}

/**
 * Делать ли добавленный по инвайту профиль активным. Первый профиль
 * приложения активен всегда, иначе главному экрану нечего показывать. А вот
 * под живым туннелем инвайт только пополняет список: пользователь пришёл
 * добавить сервер, а не оборвать текущее соединение, и переключиться он
 * может отдельным жестом.
 */
fun inviteActivatesProfile(phase: ConnectPhase, hasActiveProfile: Boolean): Boolean =
    !hasActiveProfile || phase == ConnectPhase.Idle
