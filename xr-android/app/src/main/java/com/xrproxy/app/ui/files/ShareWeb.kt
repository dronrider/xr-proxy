package com.xrproxy.app.ui.files

import java.util.Base64

/**
 * Хелперы браузерного входа и мелкой правки (LLD-33, фаза 3). Чистая
 * Kotlin-логика без Android SDK, поэтому живёт отдельным файлом от
 * [FilesViewModel] и покрывается JVM-юнитом, как [shareErrorOf].
 */

/**
 * Ссылка на вшитую web-страницу шары у агента. Токен на проводе это
 * base64url-без-дополнения блоб JSON-токена (тот же вид, что у Bearer), и
 * страница ждёт его в `?token=`. Ссылка несёт права токена целиком: из
 * write-гранта она открывает и правку, поэтому печатаем её только из-под
 * canWrite и есть отдельное предупреждение у `xr-share weblink`.
 */
fun shareWebUrl(agentBaseUrl: String, shareId: String, tokenJson: String): String {
    val blob = Base64.getUrlEncoder().withoutPadding().encodeToString(tokenJson.toByteArray())
    return "$agentBaseUrl/$shareId/web?token=$blob"
}

/**
 * Мелкая правка в приложении открывается только для текстовых файлов: двоичный
 * файл в textarea не поправить, а редактор не для контента. Список тот же, что
 * у страницы шары, чтобы приложение и браузер предлагали правку одним и тем же
 * файлам.
 */
fun isEditableTextPath(path: String): Boolean {
    // Шаг слеша без второго аргумента: без слеша возвращается вся строка,
    // а подстановка пустоты отрезала бы расширение у простого имени.
    val name = path.substringAfterLast('/')
    val ext = name.substringAfterLast('.', "").lowercase()
    return ext in EDITABLE_TEXT_EXTENSIONS
}

private val EDITABLE_TEXT_EXTENSIONS = setOf(
    "md", "markdown", "txt", "json", "toml", "yaml", "yml", "csv", "log",
)
