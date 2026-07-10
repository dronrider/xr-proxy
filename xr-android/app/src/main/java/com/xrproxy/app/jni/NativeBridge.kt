package com.xrproxy.app.jni

import android.net.Network
import com.xrproxy.app.service.XrVpnService

/**
 * JNI bridge to the Rust xr-core VPN engine.
 */
object NativeBridge {
    init {
        System.loadLibrary("xr_proxy")
    }

    /**
     * Live reference to the running XrVpnService, updated in the service
     * lifecycle (onCreate/onDestroy). Used only by the Rust-side callback
     * below, so `protectSocket` always goes through whichever service is
     * currently alive — avoids stale references after Activity recreation.
     */
    @Volatile
    var current: XrVpnService? = null

    /**
     * Underlying non-VPN network captured by `XrVpnService` via
     * `ConnectivityManager` before and during the VPN session. The VPN
     * service updates it from a `NetworkCallback` and nulls it on stop.
     *
     * Used by `resolveDomain` below to bypass the VPN tunnel when resolving
     * hostnames for direct-mode traffic — essential on whitelist networks
     * where our UDP:53 probes get dropped but the carrier's own DoT/DoH
     * channel (reached through `Network.getAllByName`) still works.
     */
    @Volatile
    var underlyingNetwork: Network? = null

    /**
     * Called FROM Rust (via JNI callback) to protect a socket fd.
     * Protected sockets bypass the VPN tunnel — critical to avoid routing loops.
     */
    @JvmStatic
    fun protectSocket(fd: Int): Boolean {
        return current?.protect(fd) ?: false
    }

    /**
     * Called FROM Rust to resolve a hostname via the underlying non-VPN
     * network. Returns an IPv4 literal or null on any failure (no underlying
     * network, unknown host, only-IPv6 result, …). Rust treats null as
     * "fall through to UDP:53 fallback."
     *
     * Blocking — Rust invokes it from `tokio::task::spawn_blocking`.
     */
    @JvmStatic
    fun resolveDomain(host: String): String? {
        val network = underlyingNetwork ?: return null
        return try {
            // We need an IPv4 for the downstream protected-TCP connect path
            // (see session.rs relay_direct IPv4 match). Iterate and pick
            // the first IPv4 answer — Android may return IPv6 first when
            // the carrier has both.
            val answers = network.getAllByName(host) ?: return null
            answers.firstOrNull { it.address.size == 4 }?.hostAddress
        } catch (_: Exception) {
            // UnknownHost, SecurityException, network-unreachable — treat
            // all as "resolver couldn't help", let Rust fall back.
            null
        }
    }

    /** Start the VPN engine. Returns null on success, or an error message on failure. */
    external fun nativeStart(tunFd: Int, configJson: String): String?
    external fun nativeStop()

    /**
     * Notify the native engine that the underlying network switched
     * (LTE↔Wi-Fi). The engine recycles the mux pool and drops live sessions so
     * the tunnel re-binds onto the new uplink within seconds, instead of
     * waiting for the slow consecutive-timeout detector. No-op if not running.
     */
    external fun nativeOnNetworkChanged()

    /**
     * True if the raw current SSID (as returned by `WifiInfo.getSSID()`,
     * quotes and all) matches any entry in `trusted`. Pure string logic in
     * Rust (`xr_core::trusted`) — case-insensitive, quote/whitespace-tolerant,
     * and treats unavailable/hidden SSIDs (`<unknown ssid>`, empty) as
     * non-matching. Safe to call whether or not the engine is running.
     */
    external fun nativeSsidMatches(currentRawSsid: String, trusted: Array<String>): Boolean

    /**
     * Normalize a raw `WifiInfo.getSSID()` value for display — strips the
     * surrounding quotes Android adds. Returns null for an unavailable/hidden
     * network (so the caller can fall back to a generic label).
     */
    external fun nativeNormalizeSsid(raw: String): String?

    external fun nativeGetState(): String
    external fun nativeGetStats(): String

    // ── Единый журнал приложения (XR-042) ───────────────────────────
    // Персистентный append-only буфер, общий для движка, проб, смен
    // сети/режима и файловых событий. Живёт на уровне процесса и на диске,
    // поэтому перезапуск движка и приложения ленту не обнуляет.

    /** Поднять журнал в [dir] (повторный вызов обновляет ротацию на лету).
     *  Вызывается из [com.xrproxy.app.XrApp] до любых других обращений. */
    external fun nativeJournalInit(dir: String, maxFileBytes: Long, maxFiles: Int)

    /** Запись из Kotlin-слоя. [level] из {"INFO","WARN","ERROR"}, [source]
     *  это короткий тег источника ("net", "probe", "vpn", "files"). */
    external fun nativeJournalLog(level: String, source: String, message: String)

    /** Хвост журнала (последние строки, от старых к новым), разделитель `\n`. */
    external fun nativeJournalTail(): String

    /** Полное содержимое журнала с диска (экспорт/шаринг). */
    external fun nativeJournalDump(): String

    /** Очистить журнал; заодно сбрасывает счётчики WARN/ERROR движка. */
    external fun nativeJournalClear()

    external fun nativePushPacket(packet: ByteArray)
    external fun nativePopPacket(): ByteArray?

    // ── Onboarding (LLD-04) ─────────────────────────────────────────
    // All functions return JSON strings — parse on Kotlin side.

    /** Parse a raw URL (scanned / pasted / deep-linked). Returns either
     *  `{"kind":"https|custom","hub_url":..,"token":..}` or `{"error":".."}`. */
    external fun nativeParseInviteLink(raw: String): String

    /** GET invite metadata (does NOT consume). Returns InviteInfo JSON
     *  (fields: token, preset, comment, status, expires_at) or `{"error":".."}`. */
    external fun nativeFetchInviteInfo(hubUrl: String, token: String, timeoutMs: Long): String

    /** Claim + TOFU public key + pre-warm preset cache. Returns JSON:
     *  `{"payload":..?,"public_key":..?,"preset_cached":bool,"errors":[..]}`.
     *  `payload` null means the whole apply failed — check `errors`. */
    external fun nativeApplyInvite(
        hubUrl: String,
        token: String,
        preset: String,
        cacheDir: String,
        timeoutMs: Long,
    ): String

    // ── APK self-update (LLD-12) ────────────────────────────────────

    /**
     * Ask the hub for a newer signed release. The manifest signature is
     * verified in Rust with the **pinned** release public key
     * ([pinnedKeyB64], compiled in via `BuildConfig.RELEASE_PUBLIC_KEY`,
     * never fetched) before anything is reported. Returns JSON:
     *  - newer available → `{"available":true,"manifest":{version_code,
     *    version_name,apk_url,apk_sha256,size_bytes,release_notes,...}}`
     *  - up-to-date / older / any failure → `{"available":false[,"error":..]}`.
     * A tampered manifest from a compromised VPS fails verification here, so
     * a forged update is never offered.
     */
    external fun nativeCheckUpdate(
        hubUrl: String,
        currentCode: Long,
        pinnedKeyB64: String,
        timeoutMs: Long,
    ): String

    /**
     * Verify a downloaded APK's SHA-256 against the value from the (already
     * signature-verified) manifest. True only on exact match; a truncated or
     * swapped download returns false and the caller deletes the file.
     */
    external fun nativeVerifyApk(path: String, sha256Hex: String): Boolean

    // ── File sharing (LLD-19) ───────────────────────────────────────
    // All functions return JSON strings — parse on Kotlin side. The mirror /
    // diff / download logic lives entirely in Rust (xr-core::sync); Kotlin only
    // supplies storage paths and a schedule. The token is a ShareToken JSON the
    // owner handed out (out-of-band); the agent verifies it offline.

    /** GET the hub's public share index. Returns
     *  `{"shares":[{share_id,name,addr,port,agent_pubkey}...]}` or `{"error":..}`. */
    external fun nativeListShares(hubUrl: String, timeoutMs: Long): String

    /** GET the shares attached to an invite (the access anchor, §9.5). Returns
     *  `{"shares":[{share_id,name,addr,port,agent_pubkey,token,exp}...]}` where
     *  `token` is the decoded ShareToken JSON ready for the manifest/download
     *  calls below. `{"error":".."}` on failure (a 410-style error = invite
     *  expired/revoked). */
    external fun nativeInviteShares(hubUrl: String, inviteToken: String, timeoutMs: Long): String

    /** Fetch a share's manifest from the agent (presents [tokenJson]). Returns
     *  `{"entries":[{path,size,mtime,sha256}...]}` or `{"error":".."}`. Used to
     *  populate the file picker for one-time download. [agentPubkey] is the
     *  identity key pinned from the grant: the agent's manifest signature is
     *  verified against it, fail-closed (XR-046). The `manifest_unsigned` /
     *  `manifest_signature` errors mean an old agent or a tampered reply. */
    external fun nativeFetchManifest(
        agentUrl: String,
        tokenJson: String,
        agentPubkey: String,
        timeoutMs: Long,
    ): String

    /** Pure diff for SAF storage. [manifestJson] is the agent manifest;
     *  [localJson] is `[{"path":..,"sha256":..}...]` the caller enumerated from
     *  the SAF tree. [selectionJson] is a JSON array of chosen manifest paths;
     *  empty/`"[]"` means the whole share. Returns the plan
     *  `{"fetch":[...],"delete":[...]}` restricted to the selection (unticked or
     *  server-gone files land in `delete`). No I/O — the caller then downloads
     *  fetches and applies deletes against the tree. */
    external fun nativePlanSync(manifestJson: String, localJson: String, selectionJson: String): String

    /** Download one manifest entry ([entryJson]) to [destDir], SHA-256-verified
     *  before it is published. Returns `{"ok":true}` or `{"error":".."}`. */
    external fun nativeDownloadFile(
        agentUrl: String,
        tokenJson: String,
        entryJson: String,
        destDir: String,
        timeoutMs: Long,
    ): String

    /** Mirror a share into [destDir] (background sync). With [dryRun] true,
     *  returns only the plan (`{"plan":{"fetch":[...],"delete":[...]}}`) so the UI
     *  can warn about deletions; with [dryRun] false it applies and also returns
     *  `{"plan":..,"report":{"fetched":[...],"deleted":[...],"failed":[...]}}`.
     *  Mirror is true-mirror: files gone on the server are deleted locally.
     *  [agentPubkey] pins the agent identity for the manifest fetch (XR-046),
     *  as in [nativeFetchManifest]. [indexPath] names the persistent hash-index
     *  file (XR-098) so a warm rescan is a stat-walk instead of re-hashing the
     *  whole share; empty = scan without an index. */
    external fun nativeSyncShare(
        agentUrl: String,
        tokenJson: String,
        agentPubkey: String,
        destDir: String,
        indexPath: String,
        selectionJson: String,
        dryRun: Boolean,
        timeoutMs: Long,
    ): String

    /** Move a share's downloaded files from [srcDir] to [dstDir] after a storage-
     *  directory change (XR-043), without re-downloading. Same-volume moves are
     *  renames; cross-volume is copy+remove, pre-checked against free space. Holds
     *  the single-transfer lock (`{"error":"busy"}` if a sync is running) and feeds
     *  the same progress as a download. Returns `{"moved":N,"bytes":N,"conflicts":
     *  [..],"failed":[[path,reason]..],"cancelled":bool}` or `{"error":".."}`. */
    external fun nativeMigrateShareDir(srcDir: String, dstDir: String): String

    /** Poll the running transfer's progress: `{active,cancelled,file,files_done,
     *  files_total,bytes_done,bytes_total}` (`active:false` when idle). */
    external fun nativeTransferProgress(): String

    /** Cancel the running sync/download (aborts at the next chunk). */
    external fun nativeCancelTransfer()
}
