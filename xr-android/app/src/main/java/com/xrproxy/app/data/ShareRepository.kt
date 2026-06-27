package com.xrproxy.app.data

import android.content.Context
import com.xrproxy.app.jni.NativeBridge
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.ShareGrant
import com.xrproxy.app.model.parseManifest
import org.json.JSONObject
import java.io.File

/**
 * File-sharing flows over the [NativeBridge] (LLD-19, XR-031). All diff/IO lives
 * in Rust (`xr-core::sync`); this composes the calls. Files land in the **app's
 * own directory** ([destDir]) — no SAF, no permissions — so the sync engine
 * works on a real filesystem path (which also fixed the empty-plan bug the SAF
 * layer caused). Every method blocks — call from a background dispatcher / Worker.
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

    /** The app-owned directory a share's files mirror into. */
    fun destDir(shareId: String): File =
        File(context.getExternalFilesDir("shares"), sanitize(shareId)).apply { mkdirs() }

    /** The local file for a share-relative path (for opening / existence checks). */
    fun fileFor(shareId: String, relPath: String): File = File(destDir(shareId), relPath)

    /** Share-relative paths already downloaded locally (drives the "downloaded" mark). */
    fun localPaths(shareId: String): Set<String> {
        val root = destDir(shareId)
        val out = HashSet<String>()
        root.walkTopDown().filter { it.isFile }.forEach {
            out.add(it.relativeTo(root).path.replace(File.separatorChar, '/'))
        }
        return out
    }

    /** Shares attached to the user's invite (the access anchor, §9.5). */
    fun inviteShares(hubUrl: String, inviteToken: String): Result<List<ShareGrant>> =
        ShareGrant.listFrom(NativeBridge.nativeInviteShares(hubUrl, inviteToken, INVITE_TIMEOUT_MS))

    /** A share's file listing from the agent (token-gated). */
    fun fetchManifest(config: ShareConfig): Result<List<ManifestEntry>> {
        val token = config.tokenJson ?: return Result.failure(IllegalStateException("no token"))
        return parseManifest(NativeBridge.nativeFetchManifest(config.agentBaseUrl, token, MANIFEST_TIMEOUT_MS))
    }

    /** One-time download of a single file into the share's app directory.
     *  Returns null on success, otherwise the error code ("busy" when another
     *  transfer is already running). */
    fun downloadOne(config: ShareConfig, entry: ManifestEntry): String? {
        val token = config.tokenJson ?: return "no token"
        val res = NativeBridge.nativeDownloadFile(
            config.agentBaseUrl, token, entry.toJson(), destDir(config.shareId).absolutePath, XFER_TIMEOUT_MS,
        )
        return runCatching {
            val o = JSONObject(res)
            if (o.optBoolean("ok", false)) null
            else o.optString("error").takeIf { it.isNotBlank() && it != "null" } ?: "download failed"
        }.getOrDefault("download failed")
    }

    /**
     * Mirror the selected subset of a share into its app directory. True mirror:
     * files that were unticked or vanished on the server are removed locally. An
     * empty selection mirrors the whole share.
     */
    fun syncOnce(config: ShareConfig): SyncOutcome {
        // Empty selection means "nothing ticked", not "mirror the whole share":
        // the background worker must not auto-download the entire share, which
        // would hold the transfer lock and block taps with a false "busy". The
        // UI mirrors only ticked files/folders.
        if (config.selection.isEmpty()) return SyncOutcome(0, 0, 0)
        val token = config.tokenJson ?: return SyncOutcome(0, 0, 0, "no token")
        val res = NativeBridge.nativeSyncShare(
            config.agentBaseUrl, token, destDir(config.shareId).absolutePath,
            config.selectionJson(), false, XFER_TIMEOUT_MS,
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

    private fun sanitize(s: String): String = s.replace(Regex("[^A-Za-z0-9_.-]"), "_")

    companion object {
        /** Invite-share listing is a quick hub metadata call; keep it short so a
         *  slow/unreachable hub clears the refresh spinner fast instead of hanging. */
        private const val INVITE_TIMEOUT_MS = 15_000L
        /** Listing is cheap on the agent (cached hashes), so a tight bound. */
        private const val MANIFEST_TIMEOUT_MS = 60_000L
        /** Transfers may be multi-GB; the engine uses a 10s connect-timeout, so a
         *  long total just bounds a genuinely stuck transfer. */
        private const val XFER_TIMEOUT_MS = 3_600_000L
    }
}
