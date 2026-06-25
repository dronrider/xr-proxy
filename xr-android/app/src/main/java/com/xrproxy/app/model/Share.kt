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
    val treeUri: String? = null,
    val syncEnabled: Boolean = false,
) {
    val agentBaseUrl: String get() = "http://$addr:$port"
    val hasToken: Boolean get() = !tokenJson.isNullOrBlank()

    fun toJson(): JSONObject = JSONObject()
        .put("share_id", shareId)
        .put("name", name)
        .put("addr", addr)
        .put("port", port)
        .put("agent_pubkey", agentPubkey)
        .put("token_json", tokenJson ?: JSONObject.NULL)
        .put("tree_uri", treeUri ?: JSONObject.NULL)
        .put("sync_enabled", syncEnabled)

    companion object {
        fun fromInfo(info: ShareInfo) = ShareConfig(
            shareId = info.shareId,
            name = info.name,
            addr = info.addr,
            port = info.port,
            agentPubkey = info.agentPubkey,
        )

        fun fromJson(o: JSONObject) = ShareConfig(
            shareId = o.getString("share_id"),
            name = o.getString("name"),
            addr = o.getString("addr"),
            port = o.getInt("port"),
            agentPubkey = o.getString("agent_pubkey"),
            tokenJson = o.optString("token_json").takeIf { it.isNotBlank() && it != "null" },
            treeUri = o.optString("tree_uri").takeIf { it.isNotBlank() && it != "null" },
            syncEnabled = o.optBoolean("sync_enabled", false),
        )
    }
}
