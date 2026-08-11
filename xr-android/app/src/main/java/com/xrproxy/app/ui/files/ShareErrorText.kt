package com.xrproxy.app.ui.files

/**
 * Что показать вместо категории ошибки: либо своя формулировка приложения, либо
 * текст, который приехал готовым. Раскладка категории в ресурс живёт в
 * Android-слое ([FilesViewModel]), поэтому здесь только выбор варианта, и файл
 * обходится без единого упоминания Android (XR-092).
 */
sealed interface ShareErrorText {
    /** Текст пришёл готовым от движка или от агента: показываем как есть. */
    data class Raw(val text: String) : ShareErrorText

    /** Своя формулировка: [kind] выбирает ресурс, [arg] подставляется в него
     *  (сейчас это только код ответа агента). */
    data class Known(val kind: ShareErrorKind, val arg: String = "") : ShareErrorText
}

/** Категории, на которые у приложения есть свои слова. */
enum class ShareErrorKind {
    AGENT_OFFLINE,
    ACCESS_EXPIRED,
    STALE_TOKEN,
    INVITE_GONE,
    NETWORK,
    NOT_FOUND,
    SERVER_ERROR,
    HTTP_STATUS,
    PARSE,
    READ,
    MANIFEST_UNSIGNED,
    MANIFEST_SIGNATURE,
    IMPORT_QUEUE_FULL,
    UNKNOWN,
}

/**
 * Ошибки из ядра приходят строкой с префиксом категории, а человеческий текст
 * это хвост после префикса (единый источник в Rust, XR-134); у устаревшего
 * токена детали по каждой ошибке нет, для него фиксированная формулировка
 * (XR-167).
 *
 * `http_*`, `parse:`, `read:` и `manifest_*` - категории, которыми отвечает
 * уже сам агент шары: прощуп прямого пути (XR-219) отсеивает чужой хост
 * раньше, чем ошибка доедет досюда. Разбор этих категорий не знал, поэтому на
 * экране всплывал сырой код вроде `http_404` (XR-243). Чистая Kotlin-логика
 * без Android SDK, поэтому раскладка живёт отдельным файлом от
 * [FilesViewModel] и покрывается JVM-юнитом, а не эмулятором.
 */
fun shareErrorOf(e: String): ShareErrorText {
    val httpStatus = if (e.startsWith("http_")) e.removePrefix("http_").toIntOrNull() else null
    return when {
        e.startsWith("agent_offline") -> tailOr(e, ShareErrorKind.AGENT_OFFLINE)
        e.startsWith("access_expired") -> tailOr(e, ShareErrorKind.ACCESS_EXPIRED)
        e.startsWith("stale_token") -> ShareErrorText.Known(ShareErrorKind.STALE_TOKEN)
        // Хаб честно ответил 410: инвайт истёк или его отозвали, а не просто
        // не открылся (XR-121). Отдельно от прочих http_* - тем текст не пишем,
        // это редкий случай, который проще разобрать по сырому коду в журнале.
        e == "http_410" -> ShareErrorText.Known(ShareErrorKind.INVITE_GONE)
        // Транспортный сбой (нет сети, агент недоступен): с офлайн-полным
        // списком качать нечего, файл ждёт сеть. Категорию наружу не тащим.
        e.startsWith("network") -> ShareErrorText.Known(ShareErrorKind.NETWORK)
        // Путь не существует именно у настоящего агента - перебор кандидатов
        // (XR-219) уже отсеял чужие хосты раньше, чем ошибка дошла сюда.
        e == "http_404" -> ShareErrorText.Known(ShareErrorKind.NOT_FOUND)
        httpStatus != null && httpStatus in 500..599 ->
            ShareErrorText.Known(ShareErrorKind.SERVER_ERROR, httpStatus.toString())
        httpStatus != null -> ShareErrorText.Known(ShareErrorKind.HTTP_STATUS, httpStatus.toString())
        e.startsWith("parse:") -> ShareErrorText.Known(ShareErrorKind.PARSE)
        // Статус пришёл, а тело оборвалось на чтении - тот же обрыв
        // соединения, что и parse:, просто пораньше (sync.rs:1634).
        e.startsWith("read:") -> ShareErrorText.Known(ShareErrorKind.READ)
        e == "manifest_unsigned" -> ShareErrorText.Known(ShareErrorKind.MANIFEST_UNSIGNED)
        e.startsWith("manifest_signature") -> ShareErrorText.Known(ShareErrorKind.MANIFEST_SIGNATURE)
        else -> ShareErrorText.Raw(e)
    }
}

/** Хвост после `категория: ` показываем как есть, а без него берём свои слова. */
private fun tailOr(e: String, kind: ShareErrorKind): ShareErrorText {
    val tail = e.substringAfter(": ", "")
    return if (tail.isBlank()) ShareErrorText.Known(kind) else ShareErrorText.Raw(tail)
}

/**
 * Ошибка импорта по URL. Категория едет тем же префиксом, а хвост после него
 * уже человеческий (единый источник в Rust), поэтому по умолчанию показывается
 * хвост. Исключение это переполненная очередь агента (429): её текст в ядре
 * написан про занятость, а на экране нужна причина отказа именно этой ссылке
 * (XR-175). Логика чистая Kotlin, живёт рядом с [shareErrorOf] и покрыта тем
 * же JVM-юнитом.
 */
fun importErrorOf(e: String): ShareErrorText = when {
    e.startsWith("queue_full") -> ShareErrorText.Known(ShareErrorKind.IMPORT_QUEUE_FULL)
    Regex("^[a-z_]+: ").containsMatchIn(e) -> ShareErrorText.Raw(e.substringAfter(": "))
    else -> ShareErrorText.Raw(e)
}

/**
 * Ошибка похожа на переходное состояние агента (форма ответа сломалась, а не
 * решение по существу): `5xx`, битое тело (`parse:`) или оборванное чтение
 * тела (`read:`) - обе идут после успешного статуса и обе входят в
 * `should_try_next` у ядра (sync.rs:1453-1454). Для них имеет смысл
 * офлайн-ретрай по бэкофу, как и для настоящей недоступности сети. `404`
 * (пути нет) и `manifest_*` (подпись не сошлась, могли ответить с чужого
 * устройства) повтором не лечатся, поэтому в список не входят (XR-243).
 */
fun isTransientAgentErrorCategory(e: String): Boolean {
    val httpStatus = if (e.startsWith("http_")) e.removePrefix("http_").toIntOrNull() else null
    return e.startsWith("parse:") || e.startsWith("read:") || (httpStatus != null && httpStatus in 500..599)
}
