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

    /**
     * Собрать JSON конфига движка из профиля активного сервера (XR-271).
     * [profileJson] это `{server_address,server_port,servers:[{name,address,
     * port}],obfuscation_key,modifier,salt,hub_url,hub_preset,hub_cache_dir,
     * user_rules:[{action,pattern}],dns_resolvers:[..],fail_closed}`. Дефолты,
     * порядок пула, чистка резолверов системы и экранирование живут в ядре, а
     * не в приложении: порт под другую платформу собирает конфиг тем же
     * вызовом. Возвращает готовый конфиг либо `{"error":".."}`.
     */
    external fun nativeBuildConfig(profileJson: String): String

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

    // -- Здоровье сессии (LLD-06, XR-271) ----------------------------
    // Скользящее окно по счётчикам движка живёт в ядре, трекер один на
    // процесс. Возвращается имя ступени: "healthy", "good", "watching",
    // "hurt", "critical".

    /** Обновить здоровье накопительными счётчиками движка (раз в тик опроса). */
    external fun nativeHealthUpdate(relayErrors: Long, relayWarnings: Long): String

    /** Сети нет: подтянуть базовую линию к текущим счётчикам, не портя
     *  здоровье ошибками, которые про отсутствие связи (XR-183). */
    external fun nativeHealthFreeze(relayErrors: Long, relayWarnings: Long): String

    /** Сброс перед новой сессией туннеля. */
    external fun nativeHealthReset()

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
     *  (fields: token, preset, comment, status, expires_at, reclaimable) or
     *  `{"error":".."}`. [cacheDir] тот же, что у [nativeApplyInvite]: оттуда
     *  берётся ключ установки, по которому хаб узнаёт потреблённый нами же
     *  инвайт и помечает его `reclaimable` (XR-216). */
    external fun nativeFetchInviteInfo(
        hubUrl: String,
        token: String,
        cacheDir: String,
        timeoutMs: Long,
    ): String

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

    // ── Редактор правил (LLD-05, XR-047) ────────────────────────────

    /** Классифицировать паттерн пользовательского правила. Валидация одна
     *  на Rust и Kotlin, UI дублирующих regex'ов не держит. Возвращает JSON
     *  `{"kind":"domain|wildcard|cidr4|cidr6","normalized":".."}` либо
     *  `{"kind":"invalid","error":"текст для пользователя"}`. */
    external fun nativeClassifyPattern(raw: String): String

    /** Форсированный fetch пресета с хаба («Обновить сейчас»). Пишет в тот же
     *  дисковый кэш, из которого движок собирает merged-роутер. Возвращает
     *  `{"updated":bool,"version":N}` либо `{"error":".."}`. */
    external fun nativeRefreshPreset(
        hubUrl: String,
        preset: String,
        cacheDir: String,
        timeoutMs: Long,
    ): String

    /** Применить правки «моих правил» к живому туннелю (XR-180): движок
     *  пересобирает merged-роутер тем же путём, каким подхватывает новую
     *  версию пресета. `rulesJson` это массив `user_rules` из конфига.
     *  Пустой `defaultAction` значит «как в конфиге старта»: своей копии
     *  этого значения приложение не держит (XR-271).
     *  `false`, когда движок не запущен (правила уедут ближайшим стартом). */
    external fun nativeApplyUserRules(rulesJson: String, defaultAction: String): Boolean

    /** Кэшированный пресет для карточки экрана правил (XR-271). Кэш пишет и
     *  читает ядро, формат файла наружу не выходит. Возвращает
     *  `{"name","version","updated_at","default_action","rules":[{"name",
     *  "action","domains","ip_ranges","geoip"}]}` либо `{"error":"no_cache"}`. */
    external fun nativeCachedPreset(cacheDir: String, preset: String): String

    /** Превью блока `[routing]` из моих правил и пресета хаба (кнопка `{ }`).
     *  Собирается в ядре рядом с кэшем пресета; пустой `defaultAction` берёт
     *  общий дефолт клиента. */
    external fun nativeMergedToml(
        cacheDir: String,
        preset: String,
        rulesJson: String,
        defaultAction: String,
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

    /** GET the hub's share index (XR-193: the index is behind auth, an empty
     *  `bearer` asks anonymously and comes back with a 401 error). `bearer` is
     *  a grant blob or an invite token. Returns
     *  `{"shares":[{share_id,name,addr,port,agent_pubkey}...]}` or `{"error":..}`. */
    external fun nativeListShares(hubUrl: String, bearer: String, timeoutMs: Long): String

    /** Публичный список пресетов хаба (сводки, XR-119). */
    external fun nativeListPresets(hubUrl: String, timeoutMs: Long): String

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
        relayJson: String,
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
     *  before it is published. Returns `{"ok":true}` or `{"error":".."}`.
     *  [agentPubkey] pins the relay's end-to-end TLS; [relayJson] is the grant's
     *  relay leg (or empty) used only when the direct address is unreachable
     *  (LLD-23 §2.4). */
    external fun nativeDownloadFile(
        agentUrl: String,
        tokenJson: String,
        entryJson: String,
        destDir: String,
        agentPubkey: String,
        relayJson: String,
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
        relayJson: String,
        dryRun: Boolean,
        timeoutMs: Long,
    ): String

    /** Start a URL-import job on a writable share (LLD-29): the agent downloads
     *  the page's content with its plugin into [dest] (share-relative folder,
     *  "" = the root). [height] is the wanted frame height, `<= 0` leaves the
     *  choice to the owner's cap. Returns `{"job_id":".."}` or `{"error":".."}`
     *  (a grant without share:import fails before any network). */
    external fun nativeImportUrl(
        addr: String,
        port: Int,
        tokenJson: String,
        agentPubkey: String,
        relayJson: String,
        url: String,
        dest: String,
        height: Int,
        timeoutMs: Long,
    ): String

    /** Poll an import job: `{"state":"queued|running|done|failed","progress":..,
     *  "files":[..],"error":".."}` or `{"error":".."}`. A job the agent forgot
     *  (restart) comes back as the named `job_lost: ...` error. */
    external fun nativeImportStatus(
        addr: String,
        port: Int,
        tokenJson: String,
        agentPubkey: String,
        relayJson: String,
        jobId: String,
        timeoutMs: Long,
    ): String

    /** Cancel an import job (the agent kills its plugin and forgets the job).
     *  Returns `{"ok":true}` or `{"error":".."}`. */
    external fun nativeImportCancel(
        addr: String,
        port: Int,
        tokenJson: String,
        agentPubkey: String,
        relayJson: String,
        jobId: String,
        timeoutMs: Long,
    ): String

    /** Delete [path] from the share itself (LLD-28, XR-250): the agent drops the
     *  file, so it leaves every holder of the share, not only this device.
     *  [expectedSha] is the hash of the manifest row the user acted on and goes
     *  out as `If-Match`, so a file replaced on the agent meanwhile answers
     *  `http_412` instead of being wiped; an empty string deletes whatever is
     *  there now. Returns `{"ok":true}` or `{"error":".."}` (`not_found`,
     *  `http_403`, `no_write_scope: ...` for a read-only grant, refused before
     *  any network). */
    external fun nativeDeleteFile(
        addr: String,
        port: Int,
        tokenJson: String,
        agentPubkey: String,
        relayJson: String,
        path: String,
        expectedSha: String,
        timeoutMs: Long,
    ): String

    /** Move a share's downloaded files from [srcDir] to [dstDir] after a storage-
     *  directory change (XR-043), without re-downloading. Same-volume moves are
     *  renames; cross-volume is copy+remove, pre-checked against free space. Holds
     *  the single-transfer lock (`{"error":"busy"}` if a sync is running) and feeds
     *  the same progress as a download. Returns `{"moved":N,"bytes":N,"conflicts":
     *  [..],"failed":[[path,reason]..],"cancelled":bool}` or `{"error":".."}`. */
    external fun nativeMigrateShareDir(srcDir: String, dstDir: String): String

    /** Drop [target] (a file or folder path) from a selection, splitting a
     *  covering folder prefix into its sibling branches (XR-044). Arguments and
     *  result are JSON string arrays (selection entries / manifest paths). Pure
     *  logic in Rust, next to the mirror planner, so both agree on what a
     *  selection entry covers. */
    external fun nativeExpandDeselect(
        selectionJson: String,
        manifestJson: String,
        target: String,
    ): String

    /** Poll the running transfer's progress: `{active,id,cancelled,share,file,
     *  files_done,files_total,bytes_done,bytes_total}` (`active:false` when
     *  idle; `share` is empty for a storage migration; `id` это номер передачи
     *  для отмены). */
    external fun nativeTransferProgress(): String

    /** Cancel the sync/download с номером [id] из снимка прогресса (aborts at
     *  the next chunk). Отмена адресная: чужую или уже законченную передачу
     *  она не трогает и отвечает `false` (XR-217). */
    external fun nativeCancelTransfer(id: Long): Boolean
}
