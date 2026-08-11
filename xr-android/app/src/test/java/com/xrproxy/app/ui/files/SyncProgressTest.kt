package com.xrproxy.app.ui.files

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * XR-056: общий индикатор очереди синка считает батч сам, а экран только
 * рисует готовую подпись. Расчёт это чистая Kotlin-логика без Android SDK,
 * поэтому он живёт в [SyncProgress.kt] и проверяется JVM-юнитом, а не
 * эмулятором.
 */
class SyncProgressTest {

    @Test
    fun idleShowsNothing() {
        assertNull(syncIndicator(SyncProgressInput()))
    }

    @Test
    fun queueCountsTheCurrentFileNotTheDoneOnes() {
        val i = syncIndicator(
            SyncProgressInput(queueSize = 3, queueDone = 2, queueHeadFile = "видео/кино/a.mkv"),
        )!!
        assertEquals("a.mkv", i.file)
        assertEquals("3 из 5", i.counter)
        assertEquals(2f / 5, i.fraction, 0.0001f)
    }

    @Test
    fun currentFileMovesTheBarInsideItsShare() {
        val i = syncIndicator(
            SyncProgressInput(
                queueSize = 1, queueDone = 0, queueHeadFile = "a.mkv",
                nativeFile = "a.mkv", nativeFilesTotal = 1, nativeFileFraction = 0.5f,
            ),
        )!!
        assertEquals("1 из 1", i.counter)
        assertEquals(0.5f, i.fraction, 0.0001f)
    }

    @Test
    fun mirrorHoldingTheLockDoesNotLendItsProgressToTheQueue() {
        // Зеркало качает своё, голова очереди ещё ждёт лока: полоса стоит на
        // готовых файлах, а не на чужих байтах.
        val i = syncIndicator(
            SyncProgressInput(
                queueSize = 2, queueDone = 0, queueHeadFile = "a.mkv",
                nativeFile = "b.mkv", nativeFilesTotal = 1, nativeFileFraction = 0.9f,
            ),
        )!!
        assertEquals("a.mkv", i.file)
        assertEquals(0f, i.fraction, 0.0001f)
    }

    @Test
    fun backgroundSyncCountsFromTheSnapshot() {
        val i = syncIndicator(
            SyncProgressInput(
                nativeFile = "музыка/b.flac", nativeFilesDone = 4, nativeFilesTotal = 10,
                // Проход зеркала отдаёт агрегатные байты, к текущему файлу они
                // отношения не имеют, поэтому экран шлёт сюда 0.
                nativeFileFraction = 0f,
            ),
        )!!
        assertEquals("b.flac", i.file)
        assertEquals("5 из 10", i.counter)
        assertEquals(0.4f, i.fraction, 0.0001f)
    }

    @Test
    fun lastFileOfTheBatchDoesNotOvercountItself() {
        val i = syncIndicator(
            SyncProgressInput(nativeFile = "b.flac", nativeFilesDone = 10, nativeFilesTotal = 10),
        )!!
        assertEquals("10 из 10", i.counter)
        assertEquals(1f, i.fraction, 0.0001f)
    }

    @Test
    fun brokenSnapshotCountsStayHidden() {
        assertNull(syncIndicator(SyncProgressInput(nativeFile = "a.mkv", nativeFilesTotal = 0)))
    }

    @Test
    fun aFinishedFileMovesTheBatchOn() {
        assertEquals(1, queueDoneAfter(done = 0, rest = 2, cancelled = false))
    }

    @Test
    fun anEmptyQueueEndsTheBatch() {
        assertEquals(0, queueDoneAfter(done = 4, rest = 0, cancelled = false))
    }

    @Test
    fun aCancelledHeadShortensTheBatchInsteadOfCounting() {
        // Крестик на качающейся голове: воркер узнаёт об отмене, когда
        // нативная передача вернётся, и засчитывать файл ему нечего.
        assertEquals(0, queueDoneAfter(done = 0, rest = 2, cancelled = true))
    }

    @Test
    fun cancellingHeadsInARowDoesNotFinishTheBatch() {
        // Очередь A, B, C, крестиком отменены A и B на закачке: батч ужимается
        // до одного оставшегося файла, а не доезжает до «3 из 3».
        var done = 0
        done = queueDoneAfter(done, rest = 2, cancelled = true)
        done = queueDoneAfter(done, rest = 1, cancelled = true)
        val i = syncIndicator(SyncProgressInput(queueSize = 1, queueDone = done, queueHeadFile = "c.mkv"))!!
        assertEquals("1 из 1", i.counter)
        assertEquals(0f, i.fraction, 0.0001f)
    }

    @Test
    fun storageMigrationKeepsItsOwnCard() {
        assertNull(
            syncIndicator(
                SyncProgressInput(nativeFile = "a.mkv", nativeFilesTotal = 3, migrating = true),
            ),
        )
    }
}
