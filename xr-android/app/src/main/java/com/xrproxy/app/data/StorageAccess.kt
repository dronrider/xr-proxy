package com.xrproxy.app.data

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.Settings
import androidx.annotation.RequiresApi
import java.io.File

/**
 * Scoped-storage helpers for the per-share storage directory (XR-043). The sync
 * engine writes a real filesystem path, so a user-picked folder needs broad
 * all-files access (MANAGE_EXTERNAL_STORAGE). The app is sideloaded, so the Play
 * restriction on that permission does not apply; the default app directory needs
 * none of this. Custom folders require Android 11+ (the permission did not exist
 * before); on Android 10 only the app directory is offered.
 */
object StorageAccess {

    /** Custom folders need MANAGE_EXTERNAL_STORAGE, introduced in Android 11. */
    fun customFolderSupported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R

    /** True once the engine may write outside the app's own dirs. */
    fun hasAllFilesAccess(): Boolean =
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && Environment.isExternalStorageManager()

    /** Settings screen to grant all-files access for this app: the user toggles
     *  it there and returns (this permission has no in-app runtime dialog). */
    @RequiresApi(Build.VERSION_CODES.R)
    fun allFilesAccessSettings(context: Context): Intent =
        Intent(
            Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
            Uri.parse("package:${context.packageName}"),
        )

    /**
     * Convert an `ACTION_OPEN_DOCUMENT_TREE` result to a real filesystem path on
     * the primary shared volume. Returns null for a non-primary volume: a
     * removable SD card has no real path the engine can write even with the
     * all-files grant, so the caller tells the user to pick on internal storage.
     */
    fun treeUriToRealPath(uri: Uri): String? {
        val docId = DocumentsContract.getTreeDocumentId(uri)
        val parts = docId.split(':', limit = 2)
        if (parts[0] != "primary") return null
        val rel = parts.getOrNull(1).orEmpty()
        return File(Environment.getExternalStorageDirectory(), rel).absolutePath
    }

    /** A short human label for where a share's files live. */
    fun label(storagePath: String?): String =
        if (storagePath == null) "Папка приложения" else prettyPath(storagePath)

    /** Trim the volume prefix for display: `/storage/emulated/0/Download/xr` ->
     *  `Download/xr`. Falls back to the full path off the primary volume. */
    private fun prettyPath(path: String): String {
        val root = Environment.getExternalStorageDirectory().absolutePath
        return if (path.startsWith(root)) {
            path.removePrefix(root).trimStart('/').ifEmpty { "Хранилище" }
        } else {
            path
        }
    }
}
