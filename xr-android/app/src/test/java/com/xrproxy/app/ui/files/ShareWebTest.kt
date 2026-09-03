package com.xrproxy.app.ui.files

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Хелперы браузерного входа и правки (LLD-33): ссылка обязана кодировать
 * токен тем же base64url-без-дополнения, каким его ждут Bearer и ?token= у
 * страницы шары, иначе агент ответит 401 на собственную ссылку. Чистая
 * Kotlin-логика, JVM-юнит без эмулятора (XR-092).
 */
class ShareWebTest {

    @Test
    fun `web url несёт base64url токен без дополнения`() {
        val token = """{"share_id":"s1","scope":"share:read share:write"}"""
        val url = shareWebUrl("http://192.0.2.10:8443", "s1", token)
        // Блоб после ?token= обязан декодироваться обратно в токен одним
        // куском: дополнение "=" или перенос строки сломали бы и агента, и
        // копирование ссылки руками.
        val blob = url.substringAfter("web?token=")
        assertFalse(blob.contains('='))
        assertFalse(blob.contains('/'))
        val decoded = java.util.Base64.getUrlDecoder().decode(blob).decodeToString()
        assertEquals(token, decoded)
        assertEquals("http://192.0.2.10:8443/s1/web?token=$blob", url)
    }

    @Test
    fun `web url ставит id шары между base url и web-путем`() {
        // Id шары это слаг хаба из безопасного алфавита, кодировать путь нечем
        // и незачем: проверяем саму сборку адреса.
        val url = shareWebUrl("http://203.0.113.7:8080", "sh2", "x")
        assertEquals("http://203.0.113.7:8080/sh2/web?token=eA", url)
    }

    @Test
    fun `правка предлагается текстовым файлам и только им`() {
        listOf(
            "notes.md", "README.markdown", "list.txt", "conf.json", "x.toml",
            "a.yaml", "b.yml", "data.csv", "app.log",
        ).forEach { assertTrue(it, isEditableTextPath(it)) }
        listOf(
            "video.mp4", "archive.tar", "image.PNG", "doc.pdf", "noext",
            "notes.md.bak", "",
        ).forEach { assertFalse(it, isEditableTextPath(it)) }
    }

    @Test
    fun `расширение правки не смотрит на путь папки`() {
        // Расширение берётся после последнего слеша и точки, поэтому папка
        // «dir.md/file» не считается текстовым файлом.
        assertFalse(isEditableTextPath("dir.md/file"))
        assertTrue(isEditableTextPath("dir/file.md"))
    }
}
