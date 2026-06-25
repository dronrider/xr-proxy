package com.xrproxy.app.data

import android.content.Context
import android.net.Uri
import android.util.Log
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareInfo
import com.xrproxy.app.model.SyncPlan
import com.xrproxy.app.model.parseManifest
import org.json.JSONObject
import java.io.File

/**
 * Orchestrates the file-sharing flows over the [NativeBridge] (LLD-19). All
 * file/diff logic is in Rust (`xr-core::sync`); this composes those calls with
 * the SAF tree I/O ([SafMirror]). Every method here is **blocking** — call from
 * a background dispatcher / Worker, never the main thread. Used by both the
 * background [com.xrproxy.app.service.ShareSyncWorker] and the UI's "sync now".
 */
class ShareRepository(private val context: Context) {

    /** Outcome of a single mirror cycle. */
    data class SyncOutcome(
        val fetched: List<String>,
        val deleted: List<String>,
        val failed: List<String>,
        val error: String? = null,
    ) {
        val ok: Boolean get() = error == null
    }

    /** Hub index of available shares. */
    fun listShares(hubUrl: String): Result<List<ShareInfo>> =
        ShareInfo.listFrom(NativeBridge.nativeListShares(hubUrl, TIMEOUT_MS))

    /** A share's file listing from the agent (token-gated). */
    fun fetchManifest(config: ShareConfig): Result<List<ManifestEntry>> {
        val token = config.tokenJson ?: return Result.failure(IllegalStateException("no token"))
        return parseManifest(NativeBridge.nativeFetchManifest(config.agentBaseUrl, token, TIMEOUT_MS))
    }

    /**
     * Download selected [entries] into the SAF [treeUri] (one-time download).
     * Each file is fetched to a temp file (Rust verifies its SHA-256) then
     * copied into the tree. Returns the list of failed paths (empty = all ok).
     */
    fun downloadInto(config: ShareConfig, entries: List<ManifestEntry>, treeUri: Uri): List<String> {
        val token = config.tokenJson ?: return entries.map { it.path }
        val tmpDir = File(context.cacheDir, "share-dl/${config.shareId}").apply { mkdirs() }
        val failed = ArrayList<String>()
        try {
            for (e in entries) {
                if (!fetchOne(config.agentBaseUrl, token, e, tmpDir, treeUri)) failed.add(e.path)
            }
        } finally {
            tmpDir.deleteRecursively()
        }
        return failed
    }

    /**
     * One full mirror cycle for a configured share: fetch manifest → enumerate
     * the local tree → diff in Rust → download new/changed, delete vanished.
     * True mirror — server deletions remove local files.
     */
    fun syncOnce(config: ShareConfig): SyncOutcome {
        val token = config.tokenJson
            ?: return SyncOutcome(emptyList(), emptyList(), emptyList(), "no token")
        val treeUri = config.treeUri?.let { Uri.parse(it) }
            ?: return SyncOutcome(emptyList(), emptyList(), emptyList(), "no folder")

        val manifestJson = NativeBridge.nativeFetchManifest(config.agentBaseUrl, token, TIMEOUT_MS)
        parseManifest(manifestJson).exceptionOrNull()?.let {
            return SyncOutcome(emptyList(), emptyList(), emptyList(), it.message ?: "manifest error")
        }

        val localJson = runCatching { SafMirror.enumerateJson(context, treeUri) }
            .getOrElse { return SyncOutcome(emptyList(), emptyList(), emptyList(), "tree error: ${it.message}") }

        val plan = SyncPlan.parse(NativeBridge.nativePlanSync(manifestJson, localJson))
            .getOrElse { return SyncOutcome(emptyList(), emptyList(), emptyList(), it.message ?: "plan error") }

        val tmpDir = File(context.cacheDir, "share-sync/${config.shareId}").apply { mkdirs() }
        val fetched = ArrayList<String>()
        val deleted = ArrayList<String>()
        val failed = ArrayList<String>()
        try {
            for (e in plan.fetch) {
                if (fetchOne(config.agentBaseUrl, token, e, tmpDir, treeUri)) fetched.add(e.path)
                else failed.add(e.path)
            }
            for (rel in plan.delete) {
                runCatching { SafMirror.deleteFile(context, treeUri, rel) }
                    .onSuccess { deleted.add(rel) }
                    .onFailure { failed.add(rel) }
            }
        } finally {
            tmpDir.deleteRecursively()
        }
        return SyncOutcome(fetched, deleted, failed)
    }

    /** Download one entry to a temp file (Rust-verified) and copy into the tree. */
    private fun fetchOne(
        agentUrl: String,
        token: String,
        entry: ManifestEntry,
        tmpDir: File,
        treeUri: Uri,
    ): Boolean {
        val res = NativeBridge.nativeDownloadFile(agentUrl, token, entry.toJson(), tmpDir.absolutePath, TIMEOUT_MS)
        if (!isOk(res)) {
            Log.w(TAG, "download ${entry.path} failed: $res")
            return false
        }
        val tmpFile = File(tmpDir, entry.path)
        return runCatching {
            SafMirror.writeFile(context, treeUri, entry.path, tmpFile)
        }.onFailure { Log.w(TAG, "write ${entry.path} into tree failed: $it") }
            .also { tmpFile.delete() }
            .isSuccess
    }

    private fun isOk(json: String): Boolean =
        runCatching { JSONObject(json).optBoolean("ok", false) }.getOrDefault(false)

    companion object {
        private const val TAG = "ShareRepository"
        private const val TIMEOUT_MS = 30_000L
    }
}
