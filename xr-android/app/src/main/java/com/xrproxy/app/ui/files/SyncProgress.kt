package com.xrproxy.app.ui.files

/**
 * Что показывает общий индикатор очереди синка (XR-056): какой файл едет
 * сейчас, какой он по счёту в батче и насколько батч пройден. Байты и скорость
 * сюда не приходят, они живут на строке файла (XR-044), а общему индикатору
 * хватает одной строки и тонкой полосы.
 */
data class SyncIndicator(
    /** Имя файла без пути. */
    val file: String,
    /** Номер текущего файла в батче, считая с единицы, а не число готовых.
     *  Подпись «X из N» собирает экран: чистой логике ресурсы не видны. */
    val current: Int,
    /** Сколько всего файлов в батче. */
    val total: Int,
    /** Доля батча, 0..1. */
    val fraction: Float,
)

/**
 * Всё, из чего складывается общий индикатор. Очередь экрана (XR-044) и
 * фоновое зеркало считаются по-разному: у очереди свой счётчик готовых, а
 * зеркало само отдаёт `files_done`/`files_total` в снимке передачи.
 */
data class SyncProgressInput(
    /** Сколько файлов ещё стоит в очереди, вместе с текущим. */
    val queueSize: Int = 0,
    /** Сколько файлов этого батча очередь уже прошла. */
    val queueDone: Int = 0,
    /** Путь головы очереди, то есть файла, который качается прямо сейчас. */
    val queueHeadFile: String? = null,
    /** Путь файла из снимка нативной передачи, null когда передачи нет. */
    val nativeFile: String? = null,
    val nativeFilesDone: Long = 0,
    val nativeFilesTotal: Long = 0,
    /**
     * Доля текущего файла по байтам, когда снимок про один файл. У прохода
     * зеркала по нескольким файлам байты в снимке агрегатные, к текущему файлу
     * отношения не имеют, поэтому оттуда сюда едет 0.
     */
    val nativeFileFraction: Float = 0f,
    /** Идёт перенос хранилища (XR-043): у него своя карточка со своим стопом. */
    val migrating: Boolean = false,
)

/**
 * Счётчик пройденных файлов батча после того, как файл ушёл из очереди
 * (XR-056). Растёт он только на честно завершённом файле, будь то докачанный
 * или сорвавшийся с ошибкой. Отменённая голова очереди счётчик не двигает, и
 * батч от этого укорачивается ровно так же, как от крестика на файле, который
 * ещё ждал своей очереди: тот уходит из очереди сам, мимо воркера. Иначе
 * отменённая голова считалась бы скачанной, и три отмены подряд уводили бы
 * подпись к «3 из 3» при пустых руках.
 *
 * [rest] это остаток очереди: опустевшая очередь заканчивает батч, и счётчик
 * обнуляется, чтобы следующий начался с нуля.
 */
fun queueDoneAfter(done: Int, rest: Int, cancelled: Boolean): Int = when {
    rest == 0 -> 0
    cancelled -> done
    else -> done + 1
}

/**
 * Свести состояние очереди и снимок передачи в одну подпись, или вернуть null,
 * когда показывать нечего. Прогресс батча считается по файлам: агрегатных байт
 * на очередь экрана никто не отдаёт (каждый файл качается своей передачей), а
 * доля текущего файла добавляется дробной частью, иначе батч из одного файла
 * держал бы полосу в нуле до самого конца.
 */
fun syncIndicator(input: SyncProgressInput): SyncIndicator? {
    if (input.migrating) return null
    val head = input.queueHeadFile
    return when {
        input.queueSize > 0 -> indicator(
            done = input.queueDone,
            total = input.queueDone + input.queueSize,
            file = head ?: input.nativeFile.orEmpty(),
            // Зеркало могло держать лок и качать свой файл: его прогресс к
            // голове очереди не относится.
            fileFraction = if (head != null && head == input.nativeFile) input.nativeFileFraction else 0f,
        )
        input.nativeFile != null && input.nativeFilesTotal > 0 -> indicator(
            done = input.nativeFilesDone.coerceIn(0, input.nativeFilesTotal).toInt(),
            total = input.nativeFilesTotal.toInt(),
            file = input.nativeFile,
            fileFraction = if (input.nativeFilesTotal == 1L) input.nativeFileFraction else 0f,
        )
        else -> null
    }
}

private fun indicator(done: Int, total: Int, file: String, fileFraction: Float): SyncIndicator {
    val whole = done.coerceIn(0, total)
    val part = fileFraction.coerceIn(0f, 1f)
    return SyncIndicator(
        file = file.substringAfterLast('/'),
        current = (whole + 1).coerceAtMost(total),
        total = total,
        fraction = ((whole + part) / total).coerceIn(0f, 1f),
    )
}
