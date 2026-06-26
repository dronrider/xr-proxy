package com.xrproxy.app.model

import org.json.JSONArray
import org.json.JSONObject

/**
 * Data model + JSON parsing for the file-sharing feature (LLD-19). The
 * NativeBridge functions return JSON strings produced by `xr-core::sync`; these
 * helpers parse them into Kotlin types. Mirrors the `org.json` style used by
 * [com.xrproxy.app.update.UpdateManager].
 */

/** A share as advertised by the hub index (`GET /api/v1/shares`). */
data class ShareInfo(
    val shareId: String,
    val name: String,
    val addr: String,
    val port: Int,
    val agentPubkey: String,
) {
    /**
     * Base URL of the agent. MVP uses plain HTTP — TLS with TOFU key-pinning of
     * the agent identity ([agentPubkey]) is a follow-up; until then the owner is
     * expected to expose the agent on a reachable http endpoint.
     */
    val agentBaseUrl: String get() = "http://$addr:$port"

    companion object {
        fun listFrom(json: String): Result<List<ShareInfo>> = runCatching {
            val obj = JSONObject(json)
            obj.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
                throw IllegalStateException(it)
            }
            val arr = obj.optJSONArray("shares") ?: JSONArray()
            (0 until arr.length()).map { i ->
                val o = arr.getJSONObject(i)
                ShareInfo(
                    shareId = o.getString("share_id"),
                    name = o.getString("name"),
                    addr = o.getString("addr"),
                    port = o.getInt("port"),
                    agentPubkey = o.getString("agent_pubkey"),
                )
            }
        }
    }
}

/**
 * A share granted to an invite holder (`GET /api/v1/invite/{token}/shares`,
 * §9.5). Unlike [ShareInfo], it carries the access [tokenJson] (minted by the
 * hub, verified by the agent offline) so no token paste is needed.
 */
data class ShareGrant(
    val shareId: String,
    val name: String,
    val addr: String,
    val port: Int,
    val agentPubkey: String,
    val tokenJson: String,
) {
    companion object {
        fun listFrom(json: String): Result<List<ShareGrant>> = runCatching {
            val obj = JSONObject(json)
            obj.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
                throw IllegalStateException(it)
            }
            val arr = obj.optJSONArray("shares") ?: JSONArray()
            (0 until arr.length()).map { i ->
                val o = arr.getJSONObject(i)
                ShareGrant(
                    shareId = o.getString("share_id"),
                    name = o.getString("name"),
                    addr = o.getString("addr"),
                    port = o.getInt("port"),
                    agentPubkey = o.getString("agent_pubkey"),
                    tokenJson = o.getString("token"),
                )
            }
        }
    }
}

/** One file in a share manifest. [sha256] is lowercase hex. */
data class ManifestEntry(
    val path: String,
    val size: Long,
    val mtime: Long,
    val sha256: String,
) {
    /** Re-serialize to the JSON shape `nativeDownloadFile` expects. */
    fun toJson(): String = JSONObject()
        .put("path", path)
        .put("size", size)
        .put("mtime", mtime)
        .put("sha256", sha256)
        .toString()

    companion object {
        fun fromJson(o: JSONObject) = ManifestEntry(
            path = o.getString("path"),
            size = o.optLong("size"),
            mtime = o.optLong("mtime"),
            sha256 = o.getString("sha256"),
        )
    }
}

/** Parsed `{"entries":[...]}` manifest, or the `{"error":..}` it may carry. */
fun parseManifest(json: String): Result<List<ManifestEntry>> = runCatching {
    val obj = JSONObject(json)
    obj.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
        throw IllegalStateException(it)
    }
    val arr = obj.optJSONArray("entries") ?: JSONArray()
    (0 until arr.length()).map { ManifestEntry.fromJson(arr.getJSONObject(it)) }
}

/** The diff `nativePlanSync` / `nativeSyncShare` returns. */
data class SyncPlan(
    val fetch: List<ManifestEntry>,
    val delete: List<String>,
) {
    val isEmpty: Boolean get() = fetch.isEmpty() && delete.isEmpty()

    companion object {
        fun fromJson(o: JSONObject): SyncPlan {
            val f = o.optJSONArray("fetch") ?: JSONArray()
            val d = o.optJSONArray("delete") ?: JSONArray()
            return SyncPlan(
                fetch = (0 until f.length()).map { ManifestEntry.fromJson(f.getJSONObject(it)) },
                delete = (0 until d.length()).map { d.getString(it) },
            )
        }

        /** Parse a bare plan JSON (from `nativePlanSync`), surfacing `error`. */
        fun parse(json: String): Result<SyncPlan> = runCatching {
            val obj = JSONObject(json)
            obj.optString("error").takeIf { it.isNotBlank() && it != "null" }?.let {
                throw IllegalStateException(it)
            }
            fromJson(obj)
        }
    }
}

/**
 * A locally-stored share the user has configured: identity from the hub, the
 * access [tokenJson] (handed out-of-band by the owner), the chosen SAF tree
 * [treeUri], and whether background mirror sync is on. Persisted as JSON.
 */
data class ShareConfig(
    val shareId: String,
    val name: String,
    val addr: String,
    val port: Int,
    val agentPubkey: String,
    val tokenJson: String? = null,
    val syncEnabled: Boolean = false,
    /** Chosen manifest paths and/or folder prefixes to mirror (§9.6). Empty =
     *  the whole share; downloads land in the app's own directory. */
    val selection: Set<String> = emptySet(),
) {
    val agentBaseUrl: String get() = "http://$addr:$port"
    val hasToken: Boolean get() = !tokenJson.isNullOrBlank()

    /** Selection as the JSON array the native sync/plan calls expect. */
    fun selectionJson(): String = JSONArray().apply { selection.forEach { put(it) } }.toString()

    fun toJson(): JSONObject = JSONObject()
        .put("share_id", shareId)
        .put("name", name)
        .put("addr", addr)
        .put("port", port)
        .put("agent_pubkey", agentPubkey)
        .put("token_json", tokenJson ?: JSONObject.NULL)
        .put("sync_enabled", syncEnabled)
        .put("selection", JSONArray().apply { selection.forEach { put(it) } })

    companion object {
        /** Build a configured share from an invite grant — the token comes with it. */
        fun fromGrant(g: ShareGrant) = ShareConfig(
            shareId = g.shareId,
            name = g.name,
            addr = g.addr,
            port = g.port,
            agentPubkey = g.agentPubkey,
            tokenJson = g.tokenJson,
        )

        fun fromJson(o: JSONObject) = ShareConfig(
            shareId = o.getString("share_id"),
            name = o.getString("name"),
            addr = o.getString("addr"),
            port = o.getInt("port"),
            agentPubkey = o.getString("agent_pubkey"),
            tokenJson = o.optString("token_json").takeIf { it.isNotBlank() && it != "null" },
            syncEnabled = o.optBoolean("sync_enabled", false),
            selection = (o.optJSONArray("selection") ?: JSONArray()).let { arr ->
                (0 until arr.length()).map { arr.getString(it) }.toSet()
            },
        )
    }
}

/** One row in the explorer: a sub-folder (navigable) or a file (downloadable). */
sealed interface TreeNode {
    val name: String
    val path: String

    /** A sub-folder at the current level; [path] is its full share-relative path. */
    data class Folder(override val name: String, override val path: String, val fileCount: Int) : TreeNode
    /** A file at the current level. */
    data class FileNode(override val name: String, val entry: ManifestEntry) : TreeNode {
        override val path: String get() = entry.path
    }
}

/**
 * Compute the immediate children of folder [dir] (`""` = root) from a flat
 * manifest: sub-folders first (alphabetical), then files. Folder-relative names
 * only, so the explorer shows one level at a time (Windows-Explorer style).
 */
fun explorerLevel(entries: List<ManifestEntry>, dir: String): List<TreeNode> {
    val prefix = if (dir.isEmpty()) "" else "$dir/"
    val folders = LinkedHashMap<String, Int>() // sub-folder name -> file count under it
    val files = ArrayList<TreeNode.FileNode>()
    for (e in entries) {
        if (!e.path.startsWith(prefix)) continue
        val rest = e.path.substring(prefix.length)
        val slash = rest.indexOf('/')
        if (slash < 0) {
            files.add(TreeNode.FileNode(rest, e))
        } else {
            val folderName = rest.substring(0, slash)
            folders[folderName] = (folders[folderName] ?: 0) + 1
        }
    }
    val out = ArrayList<TreeNode>(folders.size + files.size)
    folders.entries.sortedBy { it.key.lowercase() }.forEach { (n, c) ->
        out.add(TreeNode.Folder(n, if (dir.isEmpty()) n else "$dir/$n", c))
    }
    files.sortedBy { it.name.lowercase() }.forEach { out.add(it) }
    return out
}
