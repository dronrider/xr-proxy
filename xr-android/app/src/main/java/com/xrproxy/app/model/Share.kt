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
    /** The grant's relay leg as raw JSON (`{addr,port,obf,relay_token}`), or null
     *  for a direct share. Passed to the native calls so the consumer falls back
     *  to the relay when the direct address is unreachable (LLD-23 §2.4). */
    val relayJson: String? = null,
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
                    relayJson = o.optJSONObject("relay")?.toString(),
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
 * access [tokenJson] (handed out-of-band by the owner), where its files mirror
 * to ([storagePath]), and whether background mirror sync is on. Persisted as
 * JSON.
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
    /** Absolute filesystem directory this share's files mirror into (XR-043).
     *  `null` = the app's own per-share directory (the default, no permission).
     *  A non-null path is a user-picked folder on shared storage. */
    val storagePath: String? = null,
    /** Whether the user has made the first-sync storage choice. We prompt once,
     *  then stop asking, even if the choice was the default app directory. */
    val storageChosen: Boolean = false,
    /** The relay leg (raw JSON) for a share reachable through the hub's relay
     *  (LLD-23 §2.4), or null for a direct share. Persisted so the background
     *  mirror can fall back to the relay without re-fetching the grant. */
    val relayJson: String? = null,
) {
    val agentBaseUrl: String get() = "http://$addr:$port"
    val hasToken: Boolean get() = !tokenJson.isNullOrBlank()
    /** Relay leg for the native calls; empty string means direct-only. */
    val relayArg: String get() = relayJson ?: ""

    /** URL import is available when the hub minted share:import into the token
     *  scope (a write-binding on the invite, LLD-29); the agent holds the rest
     *  of the gates. Same whole-name semantics as Rust's scope_contains. */
    val canImport: Boolean
        get() = runCatching {
            JSONObject(tokenJson ?: return false).optString("scope")
                .split(' ').contains("share:import")
        }.getOrDefault(false)

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
        .put("storage_path", storagePath ?: JSONObject.NULL)
        .put("storage_chosen", storageChosen)
        .put("relay_json", relayJson ?: JSONObject.NULL)

    companion object {
        /** Build a configured share from an invite grant — the token comes with it. */
        fun fromGrant(g: ShareGrant) = ShareConfig(
            shareId = g.shareId,
            name = g.name,
            addr = g.addr,
            port = g.port,
            agentPubkey = g.agentPubkey,
            tokenJson = g.tokenJson,
            relayJson = g.relayJson,
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
            storagePath = o.optString("storage_path").takeIf { it.isNotBlank() && it != "null" },
            storageChosen = o.optBoolean("storage_chosen", false),
            relayJson = o.optString("relay_json").takeIf { it.isNotBlank() && it != "null" },
        )
    }
}

/** A URL-import job's state as the agent reports it (LLD-29). */
data class ImportState(
    val state: String,
    val progress: Double?,
    val files: List<String>,
    val error: String?,
) {
    val finished: Boolean get() = state == "done" || state == "failed"

    companion object {
        fun parse(json: String): Result<ImportState> = runCatching {
            val o = JSONObject(json)
            o.optString("error").takeIf { it.isNotBlank() && it != "null" && !o.has("state") }?.let {
                throw IllegalStateException(it)
            }
            ImportState(
                state = o.getString("state"),
                progress = if (o.isNull("progress")) null else o.optDouble("progress"),
                files = (o.optJSONArray("files") ?: JSONArray()).let { arr ->
                    (0 until arr.length()).map { arr.getString(it) }
                },
                error = o.optString("error").takeIf { it.isNotBlank() && it != "null" },
            )
        }
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

/** True when [path] sits under a folder present in [selection]. */
fun coveredByAncestor(path: String, selection: Set<String>): Boolean {
    var p = path
    while (true) {
        val i = p.lastIndexOf('/')
        if (i < 0) return false
        p = p.substring(0, i)
        if (selection.contains(p)) return true
    }
}

/** A path is selected if it is listed itself or sits under a listed folder. */
fun isSelected(path: String, selection: Set<String>): Boolean =
    selection.contains(path) || coveredByAncestor(path, selection)

/**
 * Remove [target] (a file or a folder) from [selection]. The direct entry and
 * everything under it just leave the set; when the target is covered by a
 * selected ancestor folder, that ancestor is split instead: every sibling
 * branch along the chain down to the target gets its own entry, the target
 * stays out. Deselecting one file therefore does not silently unselect the
 * rest of its folder.
 */
fun expandDeselect(
    selection: Set<String>,
    manifestPaths: Collection<String>,
    target: String,
): Set<String> {
    val sel = selection.toMutableSet()
    sel.removeAll { it == target || it.startsWith("$target/") }
    while (coveredByAncestor(target, sel)) {
        var ancestor = target
        while (true) {
            ancestor = ancestor.substringBeforeLast('/')
            if (sel.contains(ancestor)) break
        }
        sel.remove(ancestor)
        var dir = ancestor
        for (comp in target.substring(ancestor.length + 1).split('/')) {
            val prefix = "$dir/"
            manifestPaths.asSequence()
                .filter { it.startsWith(prefix) }
                .map { it.substring(prefix.length).substringBefore('/') }
                .distinct()
                .filter { it != comp }
                .forEach { sel.add("$dir/$it") }
            dir = "$dir/$comp"
        }
    }
    return sel
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
