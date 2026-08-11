package com.xrproxy.app.model

/**
 * Ступени здоровья сессии (LLD-06 п. 3.5a) для картинки на главном экране.
 * Считает их ядро (`xr_core::health`): окна, пороги и придержка улучшения
 * платформе не принадлежат, а переписывать их под каждый порт заново незачем
 * (XR-271). Здесь остаётся только раскладка имени ступени в то, что рисует
 * экран.
 */
enum class HealthLevel {
    /** 0 ERROR и 0 WARN за последние 30 секунд. */
    Healthy,
    /** 0 ERROR, 1-10 WARN за последние 30 секунд (фоновый шум). */
    Good,
    /** 0 ERROR, больше 10 WARN за последние 30 секунд. */
    Watching,
    /** 3 и больше ERROR в окне 30 секунд, но не всплеск. */
    Hurt,
    /** 5 и больше ERROR за последние 5 секунд (всплеск отказов). */
    Critical,
}

/** Ступень по её имени из ядра. Незнакомое имя (ядро новее приложения) это
 *  повод не пугать пользователя, а показать спокойную мордочку. */
fun healthLevelOf(name: String): HealthLevel = when (name) {
    "good" -> HealthLevel.Good
    "watching" -> HealthLevel.Watching
    "hurt" -> HealthLevel.Hurt
    "critical" -> HealthLevel.Critical
    else -> HealthLevel.Healthy
}
