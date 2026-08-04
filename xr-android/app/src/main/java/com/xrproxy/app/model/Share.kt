package com.xrproxy.app.model

import org.json.JSONArray
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Date
import java.util.Locale

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
    /** Extra reachable addresses beyond [addr] (XR-050): the agent's LAN
     *  address(es), tried before the public [addr] so a consumer on the same
     *  network reaches the agent by LAN-IP without router hairpin. */
    val addrs: List<String> = emptyList(),
    val port: Int,
    val agentPubkey: String,
    val tokenJson: String,
    /** The grant's relay leg as raw JSON (`{addr,port,obf,relay_token}`), or null
     *  for a direct share. Passed to the native calls so the consumer falls back
     *  to the relay when the direct address is unreachable (LLD-23 п. 2.4). */
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
                    addrs = stringList(o.optJSONArray("addrs")),
                    port = o.getInt("port"),
                    agentPubkey = o.getString("agent_pubkey"),
                    tokenJson = o.getString("token"),
                    relayJson = o.optJSONObject("relay")?.toString(),
                )
            }
        }
    }
}

/** Read a JSON string array into a Kotlin list (empty when absent/null). */
private fun stringList(arr: JSONArray?): List<String> =
    if (arr == null) emptyList() else (0 until arr.length()).map { arr.getString(it) }

/**
 * Откуда взялся файл (XR-255): что агент знает про страницу, с которой файл
 * скачан. Поля необязательные каждое по отдельности, у файла, залитого руками,
 * не заполнено ни одного. Едет внутри строки манифеста, то есть под подписью
 * агента: канал и ссылка тут такие же неподделываемые, как хеш.
 */
data class FileMeta(
    /** Страница, откуда файл скачан. */
    val url: String = "",
    /** Имя источника: для ролика это канал, для остального сайт или автор. */
    val source: String = "",
    /** Адрес страницы самого источника, для перехода в браузер. */
    val sourceUrl: String = "",
    /** Дата публикации в виде `ГГГГ-ММ-ДД`, не дата появления файла у агента. */
    val published: String = "",
    /** Заголовок со страницы; имя файла из него выведено, но подчищено. */
    val title: String = "",
) {
    val isEmpty: Boolean
        get() = url.isBlank() && source.isBlank() && sourceUrl.isBlank() &&
            published.isBlank() && title.isBlank()

    fun toJson(): JSONObject = JSONObject()
        .put("url", url)
        .put("source", source)
        .put("source_url", sourceUrl)
        .put("published", published)
        .put("title", title)

    companion object {
        fun fromJson(o: JSONObject) = FileMeta(
            url = o.optString("url"),
            source = o.optString("source"),
            sourceUrl = o.optString("source_url"),
            published = o.optString("published"),
            title = o.optString("title"),
        )
    }
}

/** One file in a share manifest. [sha256] is lowercase hex. */
data class ManifestEntry(
    val path: String,
    val size: Long,
    val mtime: Long,
    val sha256: String,
    /** Источник файла (XR-255), либо null: у файла без импорта поля нет и в
     *  манифесте. */
    val meta: FileMeta? = null,
) {
    /** Re-serialize to the JSON shape `nativeDownloadFile` expects. Источник
     *  едет вместе со строкой: тем же методом пишется кэш манифеста (XR-099), и
     *  без него офлайн-заход терял бы ссылку на канал. */
    fun toJson(): String = JSONObject()
        .put("path", path)
        .put("size", size)
        .put("mtime", mtime)
        .put("sha256", sha256)
        .also { o -> meta?.let { o.put("meta", it.toJson()) } }
        .toString()

    companion object {
        fun fromJson(o: JSONObject) = ManifestEntry(
            path = o.getString("path"),
            size = o.optLong("size"),
            mtime = o.optLong("mtime"),
            sha256 = o.getString("sha256"),
            meta = o.optJSONObject("meta")?.let { FileMeta.fromJson(it) }?.takeIf { !it.isEmpty },
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
    /** Extra reachable addresses beyond [addr] (XR-050): the agent's LAN
     *  address(es), tried before the public [addr] (LAN-IP without router
     *  hairpin). Persisted so the background mirror walks them too. */
    val addrs: List<String> = emptyList(),
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

    /** Candidate agent base URLs for the native sync/manifest/download calls
     *  (XR-050): LAN [addrs] first, then the public [addr], deduped and
     *  newline-joined. `xr-core::sync` splits this and walks the list, taking the
     *  first reachable address before the relay. A single-address share is one URL,
     *  identical to [agentBaseUrl]. */
    val agentBaseUrls: String
        get() = (addrs + addr)
            .map { it.trim() }
            .filter { it.isNotEmpty() }
            .distinct()
            .joinToString("\n") { "http://$it:$port" }

    /** The address argument for the native import calls (XR-050): the candidate
     *  addresses in walk order (LAN [addrs] first, public [addr] last),
     *  newline-joined. Same ordering as [agentBaseUrls]; the native side rebuilds
     *  a grant from it and walks LAN before public. */
    val importAddrArg: String
        get() = (addrs + addr)
            .map { it.trim() }
            .filter { it.isNotEmpty() }
            .distinct()
            .joinToString("\n")

    /** URL import is available when the hub minted share:import into the token
     *  scope (a write-binding on the invite, LLD-29); the agent holds the rest
     *  of the gates. Same whole-name semantics as Rust's scope_contains. */
    val canImport: Boolean get() = hasScope("share:import")

    /** Deleting a file from the share needs share:write in the token scope
     *  (LLD-28): without it the row offers no such action, and the native call
     *  would refuse before the network anyway. The agent still holds its own
     *  gates (writable share, safepath). */
    val canWrite: Boolean get() = hasScope("share:write")

    /** Whole-name scope check, as Rust's scope_contains does it: the scope is a
     *  space-separated list of names, and a prefix is not a match. */
    private fun hasScope(name: String): Boolean = runCatching {
        JSONObject(tokenJson ?: return false).optString("scope").split(' ').contains(name)
    }.getOrDefault(false)

    /** Selection as the JSON array the native sync/plan calls expect. */
    fun selectionJson(): String = JSONArray().apply { selection.forEach { put(it) } }.toString()

    fun toJson(): JSONObject = JSONObject()
        .put("share_id", shareId)
        .put("name", name)
        .put("addr", addr)
        .put("addrs", JSONArray().apply { addrs.forEach { put(it) } })
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
            addrs = g.addrs,
            port = g.port,
            agentPubkey = g.agentPubkey,
            tokenJson = g.tokenJson,
            relayJson = g.relayJson,
        )

        fun fromJson(o: JSONObject) = ShareConfig(
            shareId = o.getString("share_id"),
            name = o.getString("name"),
            addr = o.getString("addr"),
            addrs = stringList(o.optJSONArray("addrs")),
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

/** A path is selected if it is listed itself or sits under a listed folder.
 *  Mirrors `path_selected` in `xr-core::sync` (the planner's rule); the
 *  selection-rewriting counterpart lives in Rust too (`expand_deselect`). */
fun isSelected(path: String, selection: Set<String>): Boolean =
    selection.contains(path) || coveredByAncestor(path, selection)

/** Чем сортируется уровень проводника (XR-251). */
enum class FileSort { NAME, DATE }

/**
 * Порядок строк проводника: поле и направление. Один на все шары, потому что
 * от проводника ждут одной привычки, а не настройки на каждую папку.
 */
data class SortOrder(val mode: FileSort = FileSort.NAME, val descending: Boolean = false) {
    companion object {
        /** Направление, с которого режим начинают: имена читают от «а», даты
         *  смотрят от свежих. */
        fun of(mode: FileSort) = SortOrder(mode, mode == FileSort.DATE)
    }
}

/**
 * Compute the immediate children of folder [dir] (`""` = root) from a flat
 * manifest: sub-folders first, then files. Folder-relative names only, so the
 * explorer shows one level at a time (Windows-Explorer style).
 *
 * Порядок внутри каждой из двух групп задаёт [order], папки остаются выше
 * файлов в любом режиме (XR-251). Даты у папки своей нет, поэтому по дате она
 * встаёт по самому свежему файлу внутри. Совпавшие ключи разводит имя, иначе
 * файлы одной секунды переставлялись бы от прогона к прогону.
 */
fun explorerLevel(
    entries: List<ManifestEntry>,
    dir: String,
    order: SortOrder = SortOrder(),
): List<TreeNode> {
    val prefix = if (dir.isEmpty()) "" else "$dir/"
    val folders = LinkedHashMap<String, Int>() // sub-folder name -> file count under it
    val folderMtime = HashMap<String, Long>() // sub-folder name -> newest mtime under it
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
            folderMtime[folderName] = maxOf(folderMtime[folderName] ?: 0L, e.mtime)
        }
    }
    val out = ArrayList<TreeNode>(folders.size + files.size)
    val folderRows = folders.map { (n, c) ->
        TreeNode.Folder(n, if (dir.isEmpty()) n else "$dir/$n", c)
    }
    sortRows(folderRows, order) { folderMtime[it.name] ?: 0L }.forEach { out.add(it) }
    sortRows(files, order) { it.entry.mtime }.forEach { out.add(it) }
    return out
}

/** Чем список проводника разбит на группы (XR-258). */
enum class FileGrouping { NONE, DATE, SOURCE }

/**
 * Строка списка проводника: заголовок группы либо сам узел дерева. Заголовки
 * едут в том же списке, что и строки: ленивый список рисует их так же лениво,
 * а вложенные списки на группу ему пришлось бы разворачивать целиком.
 */
sealed interface ExplorerRow {
    /** Ключ строки для ленивого списка. */
    val key: String

    /** [group] это ключ группы, а не её название: канал может называться и
     *  «Без источника», и тогда два заголовка с одним ключом уронили бы ленивый
     *  список. Нулевой байт не встречается ни в пути файла, ни в имени
     *  источника, поэтому с ключами строк заголовок не столкнётся. */
    data class Header(val title: String, val count: Int, val group: String) : ExplorerRow {
        override val key: String get() = "\u0000$group"
    }

    data class Node(val node: TreeNode) : ExplorerRow {
        override val key: String get() = node.path
    }
}

/**
 * Разложить готовый уровень проводника по группам (XR-258). Порядок внутри
 * группы приходит заданным ([explorerLevel] и фильтр непросмотренных), а
 * группировка задаёт только порядок самих групп: и по дате, и по источнику они
 * идут от свежего к старому, файлы без даты и без источника собираются своей
 * группой в конце.
 *
 * Папки в группы не ложатся: даты у папки своей нет, источника тем более, а
 * потерять их нельзя, иначе в папку не зайти. Они идут своим блоком сверху, как
 * и в списке без групп.
 */
fun explorerRows(
    nodes: List<TreeNode>,
    grouping: FileGrouping,
    now: Long = System.currentTimeMillis(),
): List<ExplorerRow> {
    if (grouping == FileGrouping.NONE) return nodes.map { ExplorerRow.Node(it) }
    val rows = ArrayList<ExplorerRow>(nodes.size + 4)
    val folders = nodes.filterIsInstance<TreeNode.Folder>()
    if (folders.isNotEmpty()) {
        rows.add(ExplorerRow.Header("Папки", folders.size, FOLDERS_KEY))
        folders.forEach { rows.add(ExplorerRow.Node(it)) }
    }
    val members = LinkedHashMap<String, MutableList<TreeNode.FileNode>>()
    val titles = HashMap<String, String>()
    val ranks = HashMap<String, Long>()
    for (file in nodes.filterIsInstance<TreeNode.FileNode>()) {
        val group = when (grouping) {
            FileGrouping.SOURCE -> sourceGroup(file.entry)
            else -> dateGroup(file.entry.mtime, now)
        }
        members.getOrPut(group.key) { ArrayList() }.add(file)
        titles[group.key] = group.title
        // Группу по источнику двигает наверх её самый свежий файл, у группы по
        // дате вес один на всю группу, поэтому максимум годится обеим.
        ranks[group.key] = maxOf(ranks[group.key] ?: Long.MIN_VALUE, group.rank)
    }
    val order = members.keys.sortedWith(
        compareByDescending<String> { ranks[it] ?: Long.MIN_VALUE }.thenBy { titles[it] },
    )
    for (key in order) {
        val files = members[key] ?: continue
        rows.add(ExplorerRow.Header(titles[key] ?: key, files.size, key))
        files.forEach { rows.add(ExplorerRow.Node(it)) }
    }
    return rows
}

/** Группа файла: ключ, заголовок и вес, которым группы расставляются. */
private data class FileGroup(val key: String, val title: String, val rank: Long)

/** Ключ группы для файлов, которым группировать нечем: имя источника приходит
 *  от агента свободным текстом, и канал по имени «none» или «Без источника»
 *  схлопнулся бы с безымянными, забрав себе их место в конце списка. Нулевой
 *  байт в этот текст не попадает, как и в путь файла. */
private const val NO_GROUP_KEY = "\u0000none"

/** Ключ блока папок: он не группа файлов и с именем источника не пересекается. */
private const val FOLDERS_KEY = "\u0000folders"

/** Источник файла как его назвал агент (XR-255): у ролика это канал. Файл без
 *  источника не прячется, а собирается в свою группу последней. */
private fun sourceGroup(entry: ManifestEntry): FileGroup {
    val source = entry.meta?.source?.trim().orEmpty()
    return if (source.isEmpty()) FileGroup(NO_GROUP_KEY, "Без источника", Long.MIN_VALUE)
    else FileGroup(source, source, entry.mtime)
}

/** Корзина даты: сегодня, текущая неделя, дальше по месяцам. Дата это mtime из
 *  манифеста, то есть когда файл появился у агента; запись без даты уходит в
 *  свою группу последней. */
private fun dateGroup(mtime: Long, now: Long): FileGroup {
    if (mtime <= 0L) return FileGroup(NO_GROUP_KEY, "Без даты", Long.MIN_VALUE)
    val stamp = mtime * 1000
    val cal = Calendar.getInstance()
    cal.timeInMillis = now
    cal.startOfDay()
    val dayStart = cal.timeInMillis
    // Неделю начинаем тем днём, каким её начинает локаль телефона.
    cal.set(Calendar.DAY_OF_WEEK, cal.firstDayOfWeek)
    val weekStart = minOf(cal.timeInMillis, dayStart)
    return when {
        stamp >= dayStart -> FileGroup("today", "Сегодня", Long.MAX_VALUE)
        stamp >= weekStart -> FileGroup("week", "На этой неделе", Long.MAX_VALUE - 1)
        else -> {
            val month = Calendar.getInstance()
            month.timeInMillis = stamp
            month.set(Calendar.DAY_OF_MONTH, 1)
            month.startOfDay()
            val start = month.timeInMillis
            FileGroup("month$start", monthLabel(month.time), start)
        }
    }
}

private fun Calendar.startOfDay() {
    set(Calendar.HOUR_OF_DAY, 0)
    set(Calendar.MINUTE, 0)
    set(Calendar.SECOND, 0)
    set(Calendar.MILLISECOND, 0)
}

/** «Июль 2026»: месяц в именительном падеже, иначе получается «июля 2026». */
private fun monthLabel(date: Date): String =
    SimpleDateFormat("LLLL yyyy", Locale.forLanguageTag("ru")).format(date)
        .replaceFirstChar { it.uppercase() }

private fun <T : TreeNode> sortRows(rows: List<T>, order: SortOrder, mtime: (T) -> Long): List<T> {
    val byName = compareBy<T> { it.name.lowercase() }
    val primary = when (order.mode) {
        FileSort.NAME -> byName
        FileSort.DATE -> compareBy<T> { mtime(it) }
    }
    return rows.sortedWith(if (order.descending) primary.reversed().then(byName) else primary.then(byName))
}
