package com.xrproxy.app.ui.files

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Регресс XR-243: разбор не знал категорий `http_*`, `parse:`, `read:` и
 * `manifest_*`, которыми уже сам агент шары отвечает после прощупа XR-219, и
 * на экране висел сырой код вроде «Манифест: http_404». Логика чистая Kotlin
 * без Android SDK, поэтому раскладка вынесена в [ShareErrorText.kt] и
 * покрывается JVM-юнитом, а не эмулятором.
 *
 * Текстов тут больше нет (XR-092): своя формулировка приложения это ключ
 * [ShareErrorKind], который экран кладёт в ресурс, а сравниваются варианты.
 */
class ShareErrorTextTest {

    @Test
    fun http404GetsItsOwnKindInsteadOfTheRawCode() {
        // До правки XR-243 эта категория падала в `else` и на экран уходил
        // сырой «http_404».
        assertEquals(ShareErrorText.Known(ShareErrorKind.NOT_FOUND), shareErrorOf("http_404"))
    }

    @Test
    fun parsePrefixGetsItsOwnKind() {
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.PARSE),
            shareErrorOf("parse: expected value at line 1 column 1"),
        )
    }

    @Test
    fun readPrefixGetsItsOwnKind() {
        // До правки эта категория тоже падала в `else` (sync.rs:1634).
        assertEquals(ShareErrorText.Known(ShareErrorKind.READ), shareErrorOf("read: body closed"))
    }

    @Test
    fun manifestCategoriesGetTheirOwnKinds() {
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.MANIFEST_UNSIGNED),
            shareErrorOf("manifest_unsigned"),
        )
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.MANIFEST_SIGNATURE),
            shareErrorOf("manifest_signature: bad"),
        )
    }

    @Test
    fun serverErrorStatusesCarryTheCodeAsTheArgument() {
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.SERVER_ERROR, "503"),
            shareErrorOf("http_503"),
        )
    }

    @Test
    fun unlistedHttpCodeStillCarriesItsCode() {
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.HTTP_STATUS, "409"),
            shareErrorOf("http_409"),
        )
    }

    @Test
    fun existingCategoriesKeepWorking() {
        assertEquals(ShareErrorText.Known(ShareErrorKind.NETWORK), shareErrorOf("network: unreachable"))
        assertEquals(ShareErrorText.Known(ShareErrorKind.INVITE_GONE), shareErrorOf("http_410"))
        assertEquals(ShareErrorText.Known(ShareErrorKind.AGENT_OFFLINE), shareErrorOf("agent_offline"))
        assertEquals(ShareErrorText.Known(ShareErrorKind.STALE_TOKEN), shareErrorOf("stale_token"))
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.ACCESS_EXPIRED),
            shareErrorOf("access_expired"),
        )
    }

    /** Хвост после «категория: » пишет ядро, и он уже человеческий: показываем
     *  его как есть, а свои слова остаются на случай голой категории. */
    @Test
    fun theEngineTailWinsOverOurOwnWording() {
        assertEquals(
            ShareErrorText.Raw("агент шары не отвечает с полудня"),
            shareErrorOf("agent_offline: агент шары не отвечает с полудня"),
        )
    }

    @Test
    fun unknownCategoryFallsThroughUnchanged() {
        assertEquals(ShareErrorText.Raw("cancelled"), shareErrorOf("cancelled"))
    }

    // -- импорт по URL ----------------------------------------------------

    @Test
    fun fullImportQueueGetsItsOwnKind() {
        // 429 от агента: очередь импорта забита, и текст ядра про занятость
        // пользователю ничего не подсказывает (XR-175).
        assertEquals(
            ShareErrorText.Known(ShareErrorKind.IMPORT_QUEUE_FULL),
            importErrorOf("queue_full: очередь импорта занята, попробуй позже"),
        )
    }

    @Test
    fun otherImportCategoriesKeepTheirHumanTail() {
        assertEquals(
            ShareErrorText.Raw("нет плагина под эту ссылку"),
            importErrorOf("no_plugin: нет плагина под эту ссылку"),
        )
        assertEquals(
            ShareErrorText.Raw("агент перезапустился, задание потерялось"),
            importErrorOf("job_lost: агент перезапустился, задание потерялось"),
        )
        // Текст без машинного префикса едет как есть: так приходит хвост
        // stderr плагина у сорвавшейся джобы.
        assertEquals(ShareErrorText.Raw("ffmpeg not found"), importErrorOf("ffmpeg not found"))
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
