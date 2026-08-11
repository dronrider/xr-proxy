package com.xrproxy.app.model

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test
import java.util.Calendar
import java.util.Locale
import java.util.TimeZone

/**
 * Раскладка уровня проводника по группам (XR-258). Логика чистая, Android SDK
 * ей не нужен, а края у неё живые: граница суток и недели, дата из будущего,
 * запись без даты, пустой и пробельный источник, порядок групп по свежести.
 *
 * Время и локаль зафиксированы: от локали зависит, каким днём начинается
 * неделя и на каком языке подписан месяц, и без фиксации тест краснел бы на
 * другой машине.
 */
class ShareGroupingTest {

    private val savedLocale = Locale.getDefault()
    private val savedZone = TimeZone.getDefault()

    /** Среда, 5 августа 2026 года, полдень. */
    private val now = at(2026, 8, 5, 12, 0)

    @Before
    fun fixTimeAndLocale() {
        TimeZone.setDefault(TimeZone.getTimeZone("Europe/Moscow"))
        // Локаль со страной, а не голым языком: начало недели берётся из
        // региона, и у «ru» без региона оно воскресное, как у «en-US».
        Locale.setDefault(Locale.forLanguageTag("ru-RU"))
    }

    @After
    fun restoreTimeAndLocale() {
        TimeZone.setDefault(savedZone)
        Locale.setDefault(savedLocale)
    }

    @Test
    fun withoutGroupingRowsStayAsTheyCame() {
        val nodes = listOf(
            folder("видео"),
            file("ролик.mp4", at(2026, 8, 5, 10, 0), "Ясно и по делу"),
            file("заметка.txt", 0),
        )
        assertEquals(
            listOf("видео", "ролик.mp4", "заметка.txt"),
            render(explorerRows(nodes, FileGrouping.NONE, now)),
        )
    }

    @Test
    fun dateGroupsRunFromFreshToOld() {
        val nodes = listOf(
            file("полночь.mp4", at(2026, 8, 5, 0, 0)),
            file("вечер.mp4", at(2026, 8, 5, 23, 0)),
            file("понедельник.mp4", at(2026, 8, 3, 0, 0)),
            file("воскресенье.mp4", at(2026, 8, 2, 23, 59)),
            file("июльский.mp4", at(2026, 7, 28, 12, 0)),
            file("июньский.mp4", at(2026, 6, 15, 12, 0)),
            file("без даты.mp4", 0),
        )
        assertEquals(
            listOf(
                "# TODAY 2", "полночь.mp4", "вечер.mp4",
                "# THIS_WEEK 1", "понедельник.mp4",
                "# Август 2026 1", "воскресенье.mp4",
                "# Июль 2026 1", "июльский.mp4",
                "# Июнь 2026 1", "июньский.mp4",
                "# NO_DATE 1", "без даты.mp4",
            ),
            render(explorerRows(nodes, FileGrouping.DATE, now)),
        )
    }

    /** Тот же воскресный файл при локали, где неделя начинается воскресеньем,
     *  попадает уже в текущую неделю, а не в свой месяц. */
    @Test
    fun weekStartsWhereTheLocaleStartsIt() {
        Locale.setDefault(Locale.US)
        val nodes = listOf(file("воскресенье.mp4", at(2026, 8, 2, 23, 59)))
        assertEquals(
            listOf("# THIS_WEEK 1", "воскресенье.mp4"),
            render(explorerRows(nodes, FileGrouping.DATE, now)),
        )
    }

    /** Часы агента могут уйти вперёд. Файл с датой из будущего это всё равно
     *  самое свежее, что есть, и «Без даты» ему не место. */
    @Test
    fun stampFromTheFutureFallsIntoToday() {
        val nodes = listOf(file("завтрашний.mp4", at(2026, 8, 6, 9, 0)))
        assertEquals(
            listOf("# TODAY 1", "завтрашний.mp4"),
            render(explorerRows(nodes, FileGrouping.DATE, now)),
        )
    }

    /** Порядок групп задаёт самый свежий файл группы, а не первый попавшийся:
     *  у «Сетей без магии» свежий файл новее единственного файла соседа, хотя
     *  второй её файл старше. Порядок внутри группы приходит готовым и не
     *  пересортировывается. */
    @Test
    fun sourceGroupsRunFromTheFreshestAndNamelessGoLast() {
        val nodes = listOf(
            file("сети старое.mp4", at(2026, 7, 20, 12, 0), "Сети без магии"),
            file("сети свежее.mp4", at(2026, 8, 4, 12, 0), "Сети без магии"),
            file("ясно.mp4", at(2026, 7, 25, 12, 0), "Ясно и по делу"),
            file("пробелы.mp4", at(2026, 8, 3, 12, 0), "   "),
            file("пустая строка.mp4", at(2026, 8, 5, 9, 0), ""),
            file("без метаданных.mp4", at(2026, 6, 1, 12, 0)),
        )
        assertEquals(
            listOf(
                "# Сети без магии 2", "сети старое.mp4", "сети свежее.mp4",
                "# Ясно и по делу 1", "ясно.mp4",
                "# NO_SOURCE 3", "пробелы.mp4", "пустая строка.mp4", "без метаданных.mp4",
            ),
            render(explorerRows(nodes, FileGrouping.SOURCE, now)),
        )
    }

    /** Регресс XR-258: ключом группы безымянных была буквальная строка «none»,
     *  и канал с таким именем (source приходит от агента свободным текстом)
     *  сливался с ней в одну группу, забирая себе её место в конце списка. */
    @Test
    fun channelNamedLikeTheSentinelKeepsItsOwnGroup() {
        val nodes = listOf(
            file("ролик канала none.mp4", at(2026, 8, 5, 9, 0), "none"),
            file("безымянный.mp4", at(2026, 6, 1, 12, 0)),
        )
        assertEquals(
            listOf(
                "# none 1", "ролик канала none.mp4",
                "# NO_SOURCE 1", "безымянный.mp4",
            ),
            render(explorerRows(nodes, FileGrouping.SOURCE, now)),
        )
    }

    /** Тот же случай, только имя канала совпадает с названием группы
     *  безымянных: ключи строк обязаны остаться разными, иначе ленивый список
     *  падает на дубликате ключа. Подписи при этом приезжают разными путями,
     *  у канала это его собственное имя, у безымянных ключ своей строки
     *  ресурсов, и совпадают они только на глаз русского пользователя. */
    @Test
    fun headerKeysStayUniqueWhenChannelTakesTheGroupName() {
        val nodes = listOf(
            folder("видео"),
            file("ролик канала.mp4", at(2026, 8, 5, 9, 0), "Без источника"),
            file("безымянный.mp4", at(2026, 6, 1, 12, 0)),
        )
        val rows = explorerRows(nodes, FileGrouping.SOURCE, now)
        val keys = rows.map { it.key }
        assertEquals(keys.size, keys.toSet().size)
        assertEquals(
            listOf(
                "# FOLDERS 1", "видео",
                "# Без источника 1", "ролик канала.mp4",
                "# NO_SOURCE 1", "безымянный.mp4",
            ),
            render(rows),
        )
    }

    /** Папки в группы не ложатся ни в одном режиме: своей даты и своего
     *  источника у них нет, а потерять их нельзя, иначе в них не зайти. */
    @Test
    fun foldersKeepTheirOwnBlockOnTop() {
        val nodes = listOf(
            folder("видео"),
            folder("архив"),
            file("ролик.mp4", at(2026, 7, 1, 12, 0), "Ясно и по делу"),
        )
        assertEquals(
            listOf("# FOLDERS 2", "видео", "архив", "# Ясно и по делу 1", "ролик.mp4"),
            render(explorerRows(nodes, FileGrouping.SOURCE, now)),
        )
        assertEquals(
            listOf("# FOLDERS 2", "видео", "архив", "# Июль 2026 1", "ролик.mp4"),
            render(explorerRows(nodes, FileGrouping.DATE, now)),
        )
    }

    @Test
    fun emptyLevelGivesNoHeaders() {
        assertEquals(emptyList<String>(), render(explorerRows(emptyList(), FileGrouping.DATE, now)))
        assertEquals(emptyList<String>(), render(explorerRows(emptyList(), FileGrouping.SOURCE, now)))
    }

    // -- вспомогательное -------------------------------------------------

    /** Момент времени в зафиксированной тестом зоне, в миллисекундах. */
    private fun at(year: Int, month: Int, day: Int, hour: Int, minute: Int): Long {
        val cal = Calendar.getInstance(TimeZone.getTimeZone("Europe/Moscow"))
        cal.clear()
        cal.set(year, month - 1, day, hour, minute, 0)
        return cal.timeInMillis
    }

    /** Файл уровня: [stamp] в миллисекундах, ноль это запись без даты. */
    private fun file(name: String, stamp: Long, source: String? = null) = TreeNode.FileNode(
        name,
        ManifestEntry(
            path = name,
            size = 1,
            mtime = stamp / 1000,
            sha256 = "00",
            meta = source?.let { FileMeta(source = it) },
        ),
    )

    private fun folder(name: String) = TreeNode.Folder(name, name, 1)

    /** Строки в читаемом виде: заголовок как «# Подпись счётчик», файл именем.
     *  Своя подпись группы приезжает ключом (текст ей подставляет экран), и в
     *  ожиданиях теста стоит имя этого ключа, а не русские слова. */
    private fun render(rows: List<ExplorerRow>): List<String> = rows.map { row ->
        when (row) {
            is ExplorerRow.Header -> "# ${label(row.title)} ${row.count}"
            is ExplorerRow.Node -> row.node.name
        }
    }

    private fun label(title: GroupTitle): String = when (title) {
        is GroupTitle.Known -> title.kind.name
        is GroupTitle.Text -> title.text
    }
}
