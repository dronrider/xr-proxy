package com.xrproxy.app.data

import android.content.Context
import android.net.Uri
import androidx.documentfile.provider.DocumentFile
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.InputStream
import java.security.MessageDigest

/**
 * Reads and writes the user-chosen SAF tree for one synced share (LLD-19). The
 * diff lives in Rust (`nativePlanSync`); this is the I/O Rust cannot do because
 * a `content://` tree is not a filesystem path. We enumerate local state
 * (path + SHA-256) to feed the diff, copy verified downloads into the tree, and
 * delete files the mirror dropped.
 *
 * `DocumentFile.findFile` is O(n) per directory, so this is correctness-first;
 * for large trees a cached index would be a refinement.
 */
object SafMirror {

    /** A local file found in the tree: relative forward-slash path + hash. */
    private data class Local(val path: String, val sha256: String)

    /**
     * Enumerate the tree into the JSON array `nativePlanSync` expects:
     * `[{"path":..,"sha256":..}, ...]`. Symlinks don't exist in SAF; only files
     * are listed. Throws on an unreadable tree.
     */
    fun enumerateJson(context: Context, treeUri: Uri): String {
        val root = DocumentFile.fromTreeUri(context, treeUri)
            ?: throw IllegalStateException("cannot open tree")
        val out = ArrayList<Local>()
        walk(context, root, "", out)
        val arr = JSONArray()
        out.forEach { arr.put(JSONObject().put("path", it.path).put("sha256", it.sha256)) }
        return arr.toString()
    }

    private fun walk(context: Context, dir: DocumentFile, prefix: String, out: MutableList<Local>) {
        for (child in dir.listFiles()) {
            val name = child.name ?: continue
            val rel = if (prefix.isEmpty()) name else "$prefix/$name"
            when {
                child.isDirectory -> walk(context, child, rel, out)
                child.isFile -> {
                    val sha = context.contentResolver.openInputStream(child.uri)?.use { sha256(it) }
                        ?: continue
                    out.add(Local(rel, sha))
                }
            }
        }
    }

    /**
     * Copy [src] into the tree at [relPath] (creating intermediate directories),
     * replacing any existing file. [src] is the Rust-downloaded, already
     * SHA-256-verified temp file.
     */
    fun writeFile(context: Context, treeUri: Uri, relPath: String, src: File) {
        val root = DocumentFile.fromTreeUri(context, treeUri)
            ?: throw IllegalStateException("cannot open tree")
        val segments = relPath.split('/').filter { it.isNotEmpty() }
        if (segments.isEmpty()) return
        val fileName = segments.last()

        var dir = root
        for (seg in segments.dropLast(1)) {
            dir = dir.findFile(seg)?.takeIf { it.isDirectory }
                ?: dir.createDirectory(seg)
                ?: throw IllegalStateException("mkdir failed: $seg")
        }
        // Replace any existing file so content stays in sync.
        dir.findFile(fileName)?.delete()
        val target = dir.createFile(mimeFor(fileName), fileName)
            ?: throw IllegalStateException("create failed: $relPath")

        context.contentResolver.openOutputStream(target.uri, "w")?.use { out ->
            src.inputStream().use { it.copyTo(out) }
        } ?: throw IllegalStateException("openOutputStream failed: $relPath")
    }

    /** Delete [relPath] from the tree, then prune now-empty parent directories. */
    fun deleteFile(context: Context, treeUri: Uri, relPath: String) {
        val root = DocumentFile.fromTreeUri(context, treeUri) ?: return
        val segments = relPath.split('/').filter { it.isNotEmpty() }
        if (segments.isEmpty()) return

        // Resolve the chain so we can prune from the bottom up.
        val chain = ArrayList<DocumentFile>()
        var cur: DocumentFile? = root
        for (seg in segments) {
            cur = cur?.findFile(seg) ?: return
            chain.add(cur)
        }
        chain.last().delete()
        // Prune empty dirs (exclude the root and the just-deleted leaf).
        for (i in chain.size - 2 downTo 0) {
            val d = chain[i]
            if (d.isDirectory && d.listFiles().isEmpty()) d.delete() else break
        }
    }

    private fun sha256(input: InputStream): String {
        val md = MessageDigest.getInstance("SHA-256")
        val buf = ByteArray(64 * 1024)
        while (true) {
            val n = input.read(buf)
            if (n < 0) break
            md.update(buf, 0, n)
        }
        // `it.toInt() and 0xFF` avoids Byte sign-extension so the hex matches
        // Rust's lowercase `{:02x}` over u8 exactly.
        return md.digest().joinToString("") { "%02x".format(it.toInt() and 0xFF) }
    }

    private fun mimeFor(name: String): String {
        val ext = name.substringAfterLast('.', "").lowercase()
        return when (ext) {
            "txt", "md", "log" -> "text/plain"
            "json" -> "application/json"
            "jpg", "jpeg" -> "image/jpeg"
            "png" -> "image/png"
            "pdf" -> "application/pdf"
            else -> "application/octet-stream"
        }
    }
}
