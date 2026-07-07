package com.xrproxy.app.data

import android.content.SharedPreferences
import com.xrproxy.app.jni.NativeBridge
import java.io.File

/**
 * Параметры ротации единого журнала приложения (XR-042): размер файла и
 * сколько файлов держать (текущий + хвосты). Значения живут в общих prefs,
 * применяются нативному журналу через [apply] при старте приложения и при
 * смене настройки.
 */
object JournalSettings {
    const val KEY_MAX_KB = "journal_max_kb"
    const val KEY_MAX_FILES = "journal_max_files"

    const val DEFAULT_MAX_KB = 512
    const val DEFAULT_MAX_FILES = 3

    /** Варианты для настройки: размер одного файла журнала, КиБ. */
    val SIZE_OPTIONS_KB = listOf(128, 512, 2048)

    /** Варианты для настройки: сколько файлов держать, включая текущий. */
    val FILES_OPTIONS = listOf(1, 3, 5)

    fun maxKb(prefs: SharedPreferences): Int = prefs.getInt(KEY_MAX_KB, DEFAULT_MAX_KB)

    fun maxFiles(prefs: SharedPreferences): Int = prefs.getInt(KEY_MAX_FILES, DEFAULT_MAX_FILES)

    /** Инициализировать нативный журнал (или обновить ротацию на лету). */
    fun apply(prefs: SharedPreferences, filesDir: File) {
        NativeBridge.nativeJournalInit(
            File(filesDir, "journal").absolutePath,
            maxKb(prefs) * 1024L,
            maxFiles(prefs),
        )
    }

    fun setRotation(prefs: SharedPreferences, filesDir: File, maxKb: Int, maxFiles: Int) {
        prefs.edit().putInt(KEY_MAX_KB, maxKb).putInt(KEY_MAX_FILES, maxFiles).apply()
        apply(prefs, filesDir)
    }
}
