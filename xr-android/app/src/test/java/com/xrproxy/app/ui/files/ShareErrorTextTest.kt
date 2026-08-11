package com.xrproxy.app.ui.files

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Регресс XR-243: `humanError` не знал категорий `http_*`, `parse:`,
 * `read:` и `manifest_*`, которыми уже сам агент шары отвечает после
 * прощупа XR-219, и на экране висел сырой код вроде «Манифест: http_404».
 * Логика чистая Kotlin без Android SDK, поэтому раскладка вынесена в
 * [ShareErrorText.kt] и покрывается JVM-юнитом, а не эмулятором.
 */
class ShareErrorTextTest {

    @Test
    fun http404GetsAHumanTextInsteadOfTheRawCode() {
        // До правки эта категория падала в `else -> e` и текст был "http_404".
        val text = humanShareError("http_404")
        assertNotEquals("http_404", text)
        assertTrue(text, text.contains("404"))
        assertTrue(text, text.contains("не найден"))
    }

    @Test
    fun parsePrefixGetsAHumanText() {
        val text = humanShareError("parse: expected value at line 1 column 1")
        assertEquals("ответ сервера не разбирается", text)
    }

    @Test
    fun readPrefixGetsAHumanText() {
        // До правки эта категория тоже падала в `else -> e` (sync.rs:1634).
        val text = humanShareError("read: body closed")
        assertNotEquals("read: body closed", text)
        assertEquals("не удалось дочитать ответ сервера", text)
    }

    @Test
    fun manifestCategoriesGetHumanTexts() {
        assertNotEquals("manifest_unsigned", humanShareError("manifest_unsigned"))
        assertNotEquals(
            "manifest_signature: bad",
            humanShareError("manifest_signature: bad"),
        )
    }

    @Test
    fun serverErrorStatusesGetAHumanTextTooWithTheCodeInside() {
        val text = humanShareError("http_503")
        assertNotEquals("http_503", text)
        assertTrue(text, text.contains("503"))
    }

    @Test
    fun unlistedHttpCodeStillGetsSomeHumanWordingWithTheCode() {
        val text = humanShareError("http_409")
        assertNotEquals("http_409", text)
        assertTrue(text, text.contains("409"))
    }

    @Test
    fun existingCategoriesKeepWorking() {
        assertEquals("нет сети, попробуйте позже", humanShareError("network: unreachable"))
        assertEquals(
            "инвайт истёк или отозван, примените его заново",
            humanShareError("http_410"),
        )
        assertEquals("агент шары не на связи", humanShareError("agent_offline"))
        assertEquals(
            "токен шары устарел, обновите список по инвайту",
            humanShareError("stale_token"),
        )
    }

    @Test
    fun unknownCategoryFallsThroughUnchanged() {
        assertEquals("cancelled", humanShareError("cancelled"))
    }

    // -- офлайн-ретрай ----------------------------------------------------

    @Test
    fun serverErrorAndParseAreTransientRetryCandidates() {
        assertTrue(isTransientAgentErrorCategory("http_500"))
        assertTrue(isTransientAgentErrorCategory("http_503"))
        assertTrue(isTransientAgentErrorCategory("parse: expected value at line 1 column 1"))
        // read: тот же обрыв соединения, что и parse:, просто пораньше -
        // после статуса, но до конца тела (sync.rs:1634, should_try_next
        // считает их равноценными на sync.rs:1453-1454).
        assertTrue(isTransientAgentErrorCategory("read: body closed"))
    }

    @Test
    fun permanentAnswersAreNotRetryCandidates() {
        // 404: путь не существует у настоящего агента, повтор его не создаст.
        assertFalse(isTransientAgentErrorCategory("http_404"))
        // Ответ по существу, повтор не чинит авторизацию.
        assertFalse(isTransientAgentErrorCategory("http_401"))
        assertFalse(isTransientAgentErrorCategory("http_403"))
        // Подпись манифеста не сошлась - вопрос доверия к адресату, а не сети.
        assertFalse(isTransientAgentErrorCategory("manifest_unsigned"))
        assertFalse(isTransientAgentErrorCategory("manifest_signature: bad"))
    }
}
