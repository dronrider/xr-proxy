package com.xrproxy.app.data

import android.content.Context
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.ImportState
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareGrant
import com.xrproxy.app.model.parseManifest
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * File-sharing flows over the [NativeBridge] (LLD-19, XR-031). All diff/IO lives
 * in Rust (`xr-core::sync`); this composes the calls. Files mirror into a real
 * filesystem path so the sync engine writes directly. SAF was dropped because it
 * broke the diff with an empty plan. The path is the app's own per-share
 * directory by default, or a user-picked folder on shared storage (XR-043,
 * [ShareConfig.storagePath]). Every method blocks, so call it from a background
 * dispatcher or Worker.
 */
class ShareRepository(private val context: Context) {

    /** Outcome of a single mirror cycle. */
    data class SyncOutcome(
        val fetched: Int,
        val deleted: Int,
        val failed: Int,
        val error: String? = null,
    ) {
        val ok: Boolean get() = error == null
    }

    /** Outcome of a storage-directory migration (XR-043). */
    data class MigrateOutcome(
        val moved: Int,
        val conflicts: Int,
        val failed: Int,
        val cancelled: Boolean,
        val error: String? = null,
    ) {
        val ok: Boolean get() = error == null
    }

    /** The directory a share's files mirror into: the user-picked folder if set
     *  (XR-043), else the app's own per-share directory (the default). */
    fun destDir(config: ShareConfig): File = dirFor(config.storagePath, config.shareId).apply { mkdirs() }

    /** Resolve a (storagePath, shareId) pair to a directory without creating it.
     *  Used to name the source and target of a migration. */
    fun dirFor(storagePath: String?, shareId: String): File =
        storagePath?.let { File(it) } ?: File(context.getExternalFilesDir("shares"), sanitize(shareId))

    /** The per-share subfolder name to create inside a user-picked parent folder.
     *  A share gets its own folder so the true-mirror delete (it removes local
     *  files not in the selection) can never touch unrelated files the user keeps
     *  alongside it on shared storage. Keeps a readable name (Cyrillic, spaces),
     *  stripping only filesystem-unsafe characters; falls back to the id. */
    fun shareSubdir(config: ShareConfig): String {
        val cleaned = config.name.trim()
            .replace(Regex("[/\\\\:*?\"<>|\\x00-\\x1F]"), "_")
            .trim('.', ' ')
        return cleaned.ifBlank { sanitize(config.shareId) }
    }

    /** The local file for a share-relative path (for opening / existence checks). */
    fun fileFor(config: ShareConfig, relPath: String): File = File(destDir(config), relPath)

    /** Share-relative paths already downloaded locally (drives the "downloaded"
     *  mark). Resume partials are not downloads and stay out: listing them
     *  would adopt them into the selection and show them as files (XR-044). */
    fun localPaths(config: ShareConfig): Set<String> {
        val root = destDir(config)
        val out = HashSet<String>()
        root.walkTopDown().filter { it.isFile && !it.name.endsWith(PART_SUFFIX) }.forEach {
            out.add(it.relativeTo(root).path.replace(File.separatorChar, '/'))
        }
        return out
    }

    /** Delete one downloaded file (the per-row minus, XR-044). The caller removes
     *  the path from the selection first, otherwise the next mirror pass would
     *  fetch it right back. Also drops a resume partial of the same file and
     *  prunes directories the deletion emptied. */
    fun deleteLocal(config: ShareConfig, relPath: String) {
        val root = destDir(config)
        val f = fileFor(config, relPath)
        f.delete()
        File(f.parentFile, f.name + PART_SUFFIX).delete()
        pruneEmptyDirs(root, f.parentFile)
    }

    /** Delete every downloaded file under a share folder (folder untick, XR-044).
     *  The subtree belongs to the share alone (its own directory or the per-share
     *  subfolder on shared storage), so a recursive delete cannot reach foreign
     *  files. */
    fun deleteLocalUnder(config: ShareConfig, relDir: String) {
        val root = destDir(config)
        File(root, relDir).deleteRecursively()
        pruneEmptyDirs(root, File(root, relDir).parentFile)
    }

    /** Size of the resume partial for a path: the saved progress of a broken
     *  download, shown on the red row and picked up by the retry (XR-044).
     *  Zero when there is no partial. */
    fun partialSize(config: ShareConfig, relPath: String): Long {
        val f = fileFor(config, relPath)
        return File(f.parentFile, f.name + PART_SUFFIX).length()
    }

    /** Drop [target] from a selection, splitting a covering folder prefix into
     *  its sibling branches. The algebra lives in Rust next to the mirror
     *  planner (`sync::expand_deselect`, unit-tested there); this only
     *  marshals the sets. Falls back to removing the direct entries if the
     *  native answer does not parse. */
    fun expandDeselect(selection: Set<String>, manifestPaths: List<String>, target: String): Set<String> {
        val res = NativeBridge.nativeExpandDeselect(
            JSONArray().apply { selection.forEach { put(it) } }.toString(),
            JSONArray().apply { manifestPaths.forEach { put(it) } }.toString(),
            target,
        )
        return runCatching {
            val arr = JSONArray(res)
            (0 until arr.length()).map { arr.getString(it) }.toSet()
        }.getOrElse {
            selection.filterNot { it == target || it.startsWith("$target/") }.toSet()
        }
    }

    /** Walk up from [dir] removing directories the delete left empty, stopping
     *  at the share root (File.delete refuses non-empty ones, so this is safe). */
    private fun pruneEmptyDirs(root: File, dir: File?) {
        var d = dir
        while (d != null && d.absolutePath != root.absolutePath && d.absolutePath.startsWith(root.absolutePath)) {
            if (!d.delete()) break
            d = d.parentFile
        }
    }

    /** A manifest built from the locally-downloaded files, for offline browsing:
     *  when the agent is unreachable the already-downloaded files must stay
     *  viewable and openable. Hash is empty (unknown offline); size/mtime local. */
    fun localManifest(config: ShareConfig): List<ManifestEntry> {
        val root = destDir(config)
        return root.walkTopDown().filter { it.isFile && !it.name.endsWith(PART_SUFFIX) }.map {
            ManifestEntry(
                path = it.relativeTo(root).path.replace(File.separatorChar, '/'),
                size = it.length(),
                mtime = it.lastModified() / 1000,
                sha256 = "",
            )
        }.sortedBy { it.path }.toList()
    }

    /** Move the share's already-downloaded files into [newDir] after a storage
     *  change (XR-043). Offline, no re-download: the engine's hash diff sees the
     *  moved files as already present. A no-op when the location is unchanged. */
    fun migrateStorage(config: ShareConfig, newDir: File): MigrateOutcome {
        val src = destDir(config)
        if (src.absolutePath == newDir.absolutePath) return MigrateOutcome(0, 0, 0, false)
        val res = NativeBridge.nativeMigrateShareDir(src.absolutePath, newDir.absolutePath)
        return runCatching {
            val o = JSONObject(res)
            o.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
                return MigrateOutcome(0, 0, 0, false, it)
            }
            MigrateOutcome(
                moved = o.optInt("moved"),
                conflicts = o.optJSONArray("conflicts")?.length() ?: 0,
                failed = o.optJSONArray("failed")?.length() ?: 0,
                cancelled = o.optBoolean("cancelled", false),
            )
        }.getOrElse { MigrateOutcome(0, 0, 0, false, it.message ?: "migrate error") }
    }

    /** Shares attached to the user's invite (the access anchor, §9.5). */
    fun inviteShares(hubUrl: String, inviteToken: String): Result<List<ShareGrant>> =
        ShareGrant.listFrom(NativeBridge.nativeInviteShares(hubUrl, inviteToken, INVITE_TIMEOUT_MS))

    /** A share's file listing from the agent (token-gated). The manifest
     *  signature is verified in Rust against the pinned [ShareConfig.agentPubkey]
     *  (XR-046), so a tampered or unsigned listing surfaces here as an error. */
    fun fetchManifest(config: ShareConfig): Result<List<ManifestEntry>> {
        val token = config.tokenJson ?: return Result.failure(IllegalStateException("no token"))
        return parseManifest(
            NativeBridge.nativeFetchManifest(
                config.agentBaseUrls, token, config.agentPubkey, config.relayArg, MANIFEST_TIMEOUT_MS,
            ),
        )
    }

    /** One-time download of a single file into the share's app directory.
     *  Returns null on success, otherwise the error code ("busy" when another
     *  transfer is already running). */
    fun downloadOne(config: ShareConfig, entry: ManifestEntry): String? {
        val token = config.tokenJson ?: return "no token"
        val res = NativeBridge.nativeDownloadFile(
            config.agentBaseUrls, token, entry.toJson(), destDir(config).absolutePath,
            config.agentPubkey, config.relayArg, XFER_TIMEOUT_MS,
        )
        return runCatching {
            val o = JSONObject(res)
            if (o.optBoolean("ok", false)) null
            else o.optString("error").takeIf { it.isNotBlank() && it != "null" } ?: "download failed"
        }.getOrDefault("download failed")
    }

    /**
     * Mirror the selected subset of a share into its app directory. True mirror:
     * files that were deselected or vanished on the server are removed locally.
     *
     * An empty selection is "nothing wanted on the device", not "mirror the
     * whole share", and the pass is a no-op: auto-downloading the whole share
     * would hold the transfer lock and block the foreground queue with a false
     * "busy", and auto-deleting everything on a schedule would be worse.
     * Removing local copies is a foreground action ([deleteLocal] and
     * [deleteLocalUnder] behind the per-row controls, XR-044).
     */
    fun syncOnce(config: ShareConfig): SyncOutcome {
        if (config.selection.isEmpty()) return SyncOutcome(0, 0, 0)
        val token = config.tokenJson ?: return SyncOutcome(0, 0, 0, "no token")
        val res = NativeBridge.nativeSyncShare(
            config.agentBaseUrls, token, config.agentPubkey, destDir(config).absolutePath,
            hashIndexPath(config), config.selectionJson(), config.relayArg, false, XFER_TIMEOUT_MS,
        )
        return runCatching {
            val o = JSONObject(res)
            o.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
                return SyncOutcome(0, 0, 0, it)
            }
            val r = o.optJSONObject("report")
            SyncOutcome(
                fetched = r?.optJSONArray("fetched")?.length() ?: 0,
                deleted = r?.optJSONArray("deleted")?.length() ?: 0,
                failed = r?.optJSONArray("failed")?.length() ?: 0,
            )
        }.getOrElse { SyncOutcome(0, 0, 0, it.message ?: "sync error") }
    }

    /** Start a URL import into the share's [dest] folder (LLD-29): the agent
     *  downloads, we get a job_id and poll [importStatus]. A null [height]
     *  leaves the quality choice to the owner's cap. */
    fun importUrl(config: ShareConfig, url: String, dest: String, height: Int?): Result<String> {
        val token = config.tokenJson
            ?: return Result.failure(IllegalStateException("no token"))
        val res = NativeBridge.nativeImportUrl(
            config.importAddrArg, config.port, token, config.agentPubkey, config.relayArg,
            url, dest, height ?: 0, IMPORT_TIMEOUT_MS,
        )
        return runCatching {
            val o = JSONObject(res)
            o.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
                throw IllegalStateException(it)
            }
            o.getString("job_id")
        }
    }

    /** An import job's state; a job the agent forgot (restart) comes back as
     *  the `job_lost: ...` error, already human-worded in Rust. */
    fun importStatus(config: ShareConfig, jobId: String): Result<ImportState> {
        val token = config.tokenJson
            ?: return Result.failure(IllegalStateException("no token"))
        return ImportState.parse(
            NativeBridge.nativeImportStatus(
                config.importAddrArg, config.port, token, config.agentPubkey, config.relayArg,
                jobId, IMPORT_TIMEOUT_MS,
            ),
        )
    }

    /** Cancel an import job (the agent kills the download and forgets it). */
    fun importCancel(config: ShareConfig, jobId: String) {
        val token = config.tokenJson ?: return
        NativeBridge.nativeImportCancel(
            config.importAddrArg, config.port, token, config.agentPubkey, config.relayArg,
            jobId, IMPORT_TIMEOUT_MS,
        )
    }

    /** The share's persistent hash-index file (XR-098). Lives in the app's
     *  private [Context.getFilesDir], never inside the share directory: that one
     *  is walked by [localPaths]/[localManifest] for the UI, cleaned by the
     *  true-mirror delete, and may sit on user-visible shared storage. Keyed by
     *  shareId with share-relative entries inside, so a storage-directory change
     *  (XR-043) does not invalidate it. */
    private fun hashIndexPath(config: ShareConfig): String =
        File(File(context.filesDir, "share-index").apply { mkdirs() }, sanitize(config.shareId) + ".json")
            .absolutePath

    private fun sanitize(s: String): String = s.replace(Regex("[^A-Za-z0-9_.-]"), "_")

    companion object {
        /** Resume-partial suffix, must match `PART_SUFFIX` in `xr-core::sync`. */
        private const val PART_SUFFIX = ".xrsync-part"
        /** Invite-share listing is a quick hub metadata call; keep it short so a
         *  slow/unreachable hub clears the refresh spinner fast instead of hanging. */
        private const val INVITE_TIMEOUT_MS = 15_000L
        /** Listing is cheap on the agent (cached hashes), so a tight bound. */
        private const val MANIFEST_TIMEOUT_MS = 60_000L
        /** Transfers may be multi-GB; the engine uses a 10s connect-timeout, so a
         *  long total just bounds a genuinely stuck transfer. */
        private const val XFER_TIMEOUT_MS = 3_600_000L
        /** Starting and polling an import job are short metadata calls: the
         *  agent downloads on its own machine, not the phone, so no long
         *  timeout is needed here. */
        private const val IMPORT_TIMEOUT_MS = 30_000L
    }
}
