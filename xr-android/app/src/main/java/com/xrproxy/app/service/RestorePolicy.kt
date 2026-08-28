package com.xrproxy.app.service

/** Чем закончилась попытка поднять туннель при восстановлении (XR-279). */
enum class RestoreFailureKind {
    /** establish() не дал TUN: согласие на VPN отозвано или слот занял другой
     *  клиент. Повтор бессмыслен, пока пользователь не вернёт разрешение. */
    TUN_ESTABLISH,

    /** Движок не принял конфиг (parse, валидация): тот же конфиг даст ту же
     *  ошибку, сохранённое желание гасим, чтобы не крутить попытки при каждом
     *  старте процесса. */
    ENGINE_START,

    /** Движок поднялся, но не связался с сервером: сети ещё нет (перезагрузка),
     *  сервер временно недоступен. Повтор имеет смысл: сеть придёт. */
    ENGINE_DIED,
}

/** Восстанавливать ли туннель по сохранённому желанию пользователя: и флаг,
 *  и конфиг обязаны быть на месте, иначе восстанавливаться нечем. */
fun restoreConfigJson(wantedActive: Boolean, savedConfigJson: String?): String? =
    if (wantedActive && !savedConfigJson.isNullOrEmpty()) savedConfigJson else null

/** Гасит ли эта причина остановки сохранённое желание быть подключённым.
 *  Явное отключение в приложении гасит, системный отзыв нет: перехват слота
 *  другим VPN-клиентом или отключение в настройках Android решения
 *  пользователя не меняли, и после перезагрузки туннель возвращается (XR-221
 *  развёл эти причины, здесь на том же различении держится флаг). */
fun clearsWantedOnStop(reason: VpnStopReason): Boolean = reason == VpnStopReason.UI

/** Стоит ли повторять попытку восстановления после такого провала. */
fun shouldRetryRestore(failure: RestoreFailureKind): Boolean =
    failure == RestoreFailureKind.ENGINE_DIED

/** Пауза перед повторной попыткой восстановления: рост вдвое от попытки к
 *  попытке с потолком. Пока сети нет после перезагрузки, попытки идут редко и
 *  батарею не жгут, а когда сеть приходит, ближайшая попытка подхватывает её
 *  за потолок-минуту, а не за часы слепого ожидания. */
fun restoreRetryDelayMs(attempt: Int): Long =
    minOf(RESTORE_RETRY_BASE_MS shl attempt.coerceIn(0, 10), RESTORE_RETRY_MAX_MS)

private const val RESTORE_RETRY_BASE_MS = 5_000L
private const val RESTORE_RETRY_MAX_MS = 60_000L
