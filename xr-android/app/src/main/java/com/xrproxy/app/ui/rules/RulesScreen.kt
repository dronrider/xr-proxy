package com.xrproxy.app.ui.rules

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Rule
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Code
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.xrproxy.app.R
import com.xrproxy.app.data.CachedPreset
import com.xrproxy.app.data.UserRule
import com.xrproxy.app.data.UserRulesStore
import com.xrproxy.app.ui.PresetRefresh
import com.xrproxy.app.ui.VpnViewModel
import com.xrproxy.app.ui.components.XrPullToRefresh
import com.xrproxy.app.ui.components.XrSnackbarHost
import com.xrproxy.app.ui.UiSeverity
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import androidx.compose.foundation.layout.Box
import androidx.compose.material.icons.filled.ArrowDropDown
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.RadioButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.LaunchedEffect
import com.xrproxy.app.ui.PresetList

/**
 * Полноэкранный редактор правил маршрутизации (LLD-05, XR-047): карточка
 * пресета хаба (read-only) и упорядоченный список «моих правил», которые
 * срабатывают первыми. Открывается со вкладки «Серверы».
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RulesScreen(
    viewModel: VpnViewModel,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    val rules by viewModel.userRules.collectAsState()
    val state by viewModel.uiState.collectAsState()
    val servers by viewModel.repo.servers.collectAsState()
    val activeId by viewModel.repo.activeId.collectAsState()
    val activeServer = remember(servers, activeId) { servers.firstOrNull { it.id == activeId } }

    // Кэш пресета перечитывается по счётчику: после «Обновить сейчас» и
    // при смене активного сервера карточка видит свежий файл.
    var presetEpoch by remember { mutableIntStateOf(0) }
    val presetName = activeServer?.hubPreset.orEmpty()
    val preset by produceState<CachedPreset?>(null, presetName, presetEpoch) {
        value = withContext(Dispatchers.IO) {
            presetName.takeIf { it.isNotBlank() }?.let { viewModel.readCachedPreset(it) }
        }
    }

    val snackbarHostState = remember { SnackbarHostState() }
    var lastSeverity by remember { mutableStateOf(UiSeverity.Info) }
    val scope = rememberCoroutineScope()
    fun snack(text: String, severity: UiSeverity = UiSeverity.Info) {
        lastSeverity = severity
        scope.launch { snackbarHostState.showSnackbar(text) }
    }

    var editTarget by remember { mutableStateOf<UserRule?>(null) }
    var addDialogOpen by remember { mutableStateOf(false) }
    var tomlOpen by remember { mutableStateOf(false) }
    var detailsOpen by remember { mutableStateOf(false) }
    var refreshing by remember { mutableStateOf(false) }
    var pickerOpen by remember { mutableStateOf(false) }
    // Пресет выбираем только когда у активного сервера есть хаб (XR-119).
    val canPick = activeServer?.hubUrl?.isNotBlank() == true

    // Единая точка сохранения. Обещания «применятся при следующем
    // подключении» здесь больше нет (XR-180): правило действует на живом
    // туннеле сразу, а список сам себе подтверждение.
    fun applyRules(newRules: List<UserRule>) {
        viewModel.saveUserRules(newRules)
    }

    val presetSnapshot = preset
    if (detailsOpen && presetSnapshot != null) {
        PresetDetailsScreen(preset = presetSnapshot, onBack = { detailsOpen = false })
        return
    }

    val backLabel = stringResource(R.string.rules_back)
    Scaffold(
        snackbarHost = { XrSnackbarHost(snackbarHostState, lastSeverity) },
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.rules_title)) },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, backLabel)
                    }
                },
                actions = {
                    IconButton(onClick = { tomlOpen = true }) {
                        Icon(Icons.Default.Code, stringResource(R.string.rules_show_toml))
                    }
                },
            )
        },
    ) { padding ->
        // Обновление пресета хаба тем же жестом, что и кнопка «Обновить сейчас»
        // на карточке (стандарт XR-181): свайп-вниз перечитывает пресет с хаба.
        val refreshPreset: () -> Unit = {
            refreshing = true
            scope.launch {
                when (val r = viewModel.refreshPresetNow()) {
                    is PresetRefresh.Updated ->
                        snack(context.getString(R.string.rules_preset_updated, r.version))
                    is PresetRefresh.UpToDate ->
                        snack(context.getString(R.string.rules_preset_up_to_date, r.version))
                    is PresetRefresh.Failed ->
                        snack(r.message, UiSeverity.Error)
                }
                presetEpoch++
                refreshing = false
            }
        }
        XrPullToRefresh(
            refreshing = refreshing,
            onRefresh = refreshPreset,
            modifier = Modifier.padding(padding),
        ) {
        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(horizontal = 16.dp),
        ) {
            if (state.paused) {
                item { RulesPausedNote() }
            }
            item {
                SectionHeader(
                    stringResource(R.string.rules_server_rules_title),
                    stringResource(R.string.rules_server_rules_subtitle),
                )
                PresetCard(
                    presetName = presetName,
                    preset = preset,
                    refreshing = refreshing,
                    onRefresh = refreshPreset,
                    onDetails = { detailsOpen = true },
                    onPick = if (canPick) ({ pickerOpen = true }) else null,
                )
            }
            item {
                SectionHeader(
                    stringResource(R.string.rules_my_rules_title),
                    stringResource(R.string.rules_my_rules_subtitle),
                )
                Spacer(Modifier.height(8.dp))
            }
            items(rules, key = { it.id }) { rule ->
                UserRuleRow(
                    rule = rule,
                    isFirst = rules.firstOrNull()?.id == rule.id,
                    isLast = rules.lastOrNull()?.id == rule.id,
                    onToggleAction = {
                        applyRules(rules.map {
                            if (it.id == rule.id) {
                                it.copy(action = if (it.action == "proxy") "direct" else "proxy")
                            } else it
                        })
                    },
                    onEdit = { editTarget = rule },
                    onDelete = { applyRules(rules.filter { it.id != rule.id }) },
                    onMove = { delta -> applyRules(moveRule(rules, rule.id, delta)) },
                )
                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            }
            item {
                Spacer(Modifier.height(12.dp))
                val limitReachedText = stringResource(
                    R.string.rules_limit_reached,
                    UserRulesStore.MAX_RULES,
                )
                OutlinedButton(
                    onClick = {
                        if (rules.size >= UserRulesStore.MAX_RULES) {
                            snack(limitReachedText, UiSeverity.Warn)
                        } else {
                            addDialogOpen = true
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Icon(Icons.Default.Add, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text(stringResource(R.string.rules_add_rule))
                }
                Spacer(Modifier.height(24.dp))
            }
        }
        }
    }

    if (addDialogOpen) {
        RuleEditDialog(
            initial = null,
            onDismiss = { addDialogOpen = false },
            onSave = { rule ->
                addDialogOpen = false
                applyRules(rules + rule)
            },
        )
    }
    editTarget?.let { target ->
        RuleEditDialog(
            initial = target,
            onDismiss = { editTarget = null },
            onSave = { rule ->
                editTarget = null
                applyRules(rules.map { if (it.id == target.id) rule else it })
            },
        )
    }
    val copiedText = stringResource(R.string.rules_copied)
    val presetChangedText = stringResource(R.string.rules_preset_changed)
    if (tomlOpen) {
        // Превью собирает ядро рядом с кэшем пресета (XR-271), поэтому текст
        // приезжает не сразу: до первого ответа диалог показывает пустой блок.
        val toml by produceState("", rules, presetName, presetEpoch) {
            value = viewModel.mergedToml(rules)
        }
        TomlPreviewDialog(
            toml = toml,
            onDismiss = { tomlOpen = false },
            onCopied = { snack(copiedText) },
        )
    }
    if (pickerOpen) {
        PresetPickerSheet(
            current = presetName,
            loadPresets = { viewModel.listHubPresets() },
            onPick = { name ->
                pickerOpen = false
                viewModel.setActivePreset(name)
                presetEpoch++
                if (state.connected) {
                    snack(presetChangedText)
                }
            },
            onDismiss = { pickerOpen = false },
        )
    }
}

/** Перестановка правила: [delta] +-1 на позицию, Int.MIN_VALUE/MAX_VALUE в край. */
private fun moveRule(rules: List<UserRule>, id: String, delta: Int): List<UserRule> {
    val idx = rules.indexOfFirst { it.id == id }
    if (idx < 0) return rules
    val target = (idx + delta).coerceIn(0, rules.lastIndex)
    if (target == idx) return rules
    val mutable = rules.toMutableList()
    val rule = mutable.removeAt(idx)
    mutable.add(target, rule)
    return mutable
}

/** Заголовок секции экрана правил: название плюс поясняющая строка. */
@Composable
private fun SectionHeader(title: String, subtitle: String) {
    Text(
        title,
        style = MaterialTheme.typography.titleMedium,
        modifier = Modifier.padding(top = 16.dp, bottom = 4.dp),
    )
    Text(
        subtitle,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

/** Подпись при паузе в доверенной сети (XR-120): трафик идёт напрямую, правила
 *  не действуют, поэтому список ниже нерелевантен. */
@Composable
private fun RulesPausedNote() {
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = MaterialTheme.colorScheme.tertiary.copy(alpha = 0.12f),
        modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
    ) {
        Row(modifier = Modifier.padding(12.dp), verticalAlignment = Alignment.Top) {
            Icon(
                Icons.Default.Pause,
                null,
                tint = MaterialTheme.colorScheme.tertiary,
                modifier = Modifier.size(20.dp),
            )
            Spacer(Modifier.width(10.dp))
            Text(
                stringResource(R.string.rules_paused_note),
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

// ── Карточка пресета ────────────────────────────────────────────────

@Composable
private fun PresetCard(
    presetName: String,
    preset: CachedPreset?,
    refreshing: Boolean,
    onRefresh: () -> Unit,
    onDetails: () -> Unit,
    onPick: (() -> Unit)?,
) {
    OutlinedCard(modifier = Modifier.fillMaxWidth().padding(top = 4.dp)) {
        Column(modifier = Modifier.padding(16.dp)) {
            if (presetName.isBlank()) {
                PresetTitleRow(stringResource(R.string.rules_preset_not_connected), onPick)
                Spacer(Modifier.height(4.dp))
                Text(
                    stringResource(R.string.rules_preset_not_connected_hint),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                return@Column
            }
            PresetTitleRow(
                if (preset != null) {
                    stringResource(R.string.rules_preset_title_versioned, presetName, preset.version)
                } else {
                    stringResource(R.string.rules_preset_title, presetName)
                },
                onPick,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                if (preset != null) {
                    val date = preset.updatedAt.take(10)
                    if (date.isNotBlank()) {
                        stringResource(R.string.rules_preset_summary_with_date, preset.rules.size, date)
                    } else {
                        stringResource(R.string.rules_preset_summary, preset.rules.size)
                    }
                } else {
                    stringResource(R.string.rules_preset_not_downloaded)
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(12.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = onRefresh, enabled = !refreshing) {
                    if (refreshing) {
                        CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                        Spacer(Modifier.width(8.dp))
                    }
                    Text(stringResource(R.string.rules_refresh_now))
                }
                OutlinedButton(onClick = onDetails, enabled = preset != null) {
                    Text(stringResource(R.string.rules_details))
                }
            }
        }
    }
}

/** Заголовок карточки пресета. Когда доступен выбор (onPick != null), строка
 *  становится дропдауном с шевроном (XR-119). */
@Composable
private fun PresetTitleRow(text: String, onPick: (() -> Unit)?) {
    if (onPick == null) {
        Text(text, style = MaterialTheme.typography.titleMedium)
        return
    }
    Row(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onPick),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(text, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
        Icon(
            Icons.Default.ArrowDropDown,
            stringResource(R.string.rules_change_preset),
            tint = MaterialTheme.colorScheme.primary,
        )
    }
}

/** Bottom sheet выбора пресета из списка хаба (XR-119). Список грузится один
 *  раз при открытии; выбор пишется в профиль и применяется на следующем
 *  подключении. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PresetPickerSheet(
    current: String,
    loadPresets: suspend () -> PresetList,
    onPick: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    val sheetState = rememberModalBottomSheetState()
    var result by remember { mutableStateOf<PresetList?>(null) }
    LaunchedEffect(Unit) { result = loadPresets() }

    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = sheetState) {
        Column(modifier = Modifier.fillMaxWidth().padding(bottom = 24.dp)) {
            Text(
                stringResource(R.string.rules_pick_preset_title),
                style = MaterialTheme.typography.titleLarge,
                modifier = Modifier.padding(start = 24.dp, end = 24.dp, bottom = 8.dp),
            )
            when (val r = result) {
                null -> Box(
                    modifier = Modifier.fillMaxWidth().padding(32.dp),
                    contentAlignment = Alignment.Center,
                ) {
                    CircularProgressIndicator(Modifier.size(28.dp), strokeWidth = 3.dp)
                }
                is PresetList.Failed -> Text(
                    r.message,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                    modifier = Modifier.padding(horizontal = 24.dp, vertical = 12.dp),
                )
                is PresetList.Ok -> if (r.presets.isEmpty()) {
                    Text(
                        stringResource(R.string.rules_no_presets),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(horizontal = 24.dp, vertical = 12.dp),
                    )
                } else {
                    r.presets.forEach { p ->
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .clickable { onPick(p.name) }
                                .padding(horizontal = 20.dp, vertical = 12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            RadioButton(selected = p.name == current, onClick = { onPick(p.name) })
                            Spacer(Modifier.width(8.dp))
                            Column(modifier = Modifier.weight(1f)) {
                                Text(p.name, style = MaterialTheme.typography.bodyLarge)
                                Text(
                                    stringResource(
                                        R.string.rules_preset_item_summary,
                                        p.rulesCount,
                                        p.version,
                                    ),
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Строка моего правила ────────────────────────────────────────────

@Composable
private fun UserRuleRow(
    rule: UserRule,
    isFirst: Boolean,
    isLast: Boolean,
    onToggleAction: () -> Unit,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
    onMove: (Int) -> Unit,
) {
    var menuExpanded by remember { mutableStateOf(false) }

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(56.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        ActionPill(action = rule.action, onClick = onToggleAction)
        Spacer(Modifier.width(12.dp))
        Text(
            rule.pattern,
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodyMedium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        IconButton(onClick = { menuExpanded = true }) {
            Icon(Icons.Default.MoreVert, stringResource(R.string.rules_menu))
        }
        DropdownMenu(expanded = menuExpanded, onDismissRequest = { menuExpanded = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.rules_edit)) },
                onClick = { menuExpanded = false; onEdit() },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.rules_move_up)) },
                enabled = !isFirst,
                onClick = { menuExpanded = false; onMove(-1) },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.rules_move_down)) },
                enabled = !isLast,
                onClick = { menuExpanded = false; onMove(1) },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.rules_move_top)) },
                enabled = !isFirst,
                onClick = { menuExpanded = false; onMove(Int.MIN_VALUE / 2) },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.rules_move_bottom)) },
                enabled = !isLast,
                onClick = { menuExpanded = false; onMove(Int.MAX_VALUE / 2) },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.rules_delete), color = MaterialTheme.colorScheme.error) },
                onClick = { menuExpanded = false; onDelete() },
            )
        }
    }
}

/** Пилюля действия: тап переключает proxy <-> direct без захода в диалог. */
@Composable
private fun ActionPill(action: String, onClick: () -> Unit) {
    val isProxy = action == "proxy"
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = if (isProxy) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surface,
        contentColor = if (isProxy) MaterialTheme.colorScheme.onPrimary
        else MaterialTheme.colorScheme.onSurfaceVariant,
        border = if (isProxy) null else BorderStroke(1.dp, MaterialTheme.colorScheme.outline),
        modifier = Modifier.clickable(onClick = onClick),
    ) {
        Text(
            if (isProxy) "proxy" else "direct",
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 4.dp),
        )
    }
}

// ── Read-only просмотр пресета ──────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PresetDetailsScreen(preset: CachedPreset, onBack: () -> Unit) {
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Text(stringResource(R.string.rules_preset_details_title, preset.name, preset.version))
                },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, stringResource(R.string.rules_back))
                    }
                },
            )
        },
    ) { padding ->
        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 16.dp),
        ) {
            item {
                Text(
                    stringResource(R.string.rules_preset_readonly_hint),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(vertical = 12.dp),
                )
            }
            items(preset.rules.size) { i ->
                PresetRuleCard(preset.rules[i])
                Spacer(Modifier.height(8.dp))
            }
            item { Spacer(Modifier.height(16.dp)) }
        }
    }
}

@Composable
private fun PresetRuleCard(rule: com.xrproxy.app.data.CachedPresetRule) {
    var expanded by remember { mutableStateOf(false) }
    OutlinedCard(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { expanded = !expanded },
    ) {
        Column(modifier = Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                ActionPill(action = rule.action, onClick = { expanded = !expanded })
                Spacer(Modifier.width(12.dp))
                val domainsLabel = stringResource(R.string.rules_domains_count, rule.domains.size)
                val ipLabel = stringResource(R.string.rules_ip_count, rule.ipRanges.size)
                val geoipLabel = stringResource(R.string.rules_geoip_count, rule.geoip.size)
                val emptyRuleLabel = stringResource(R.string.rules_empty_rule)
                val summary = buildList {
                    if (rule.domains.isNotEmpty()) add(domainsLabel)
                    if (rule.ipRanges.isNotEmpty()) add(ipLabel)
                    if (rule.geoip.isNotEmpty()) add(geoipLabel)
                }.joinToString(" \u00B7 ")
                // Имя тематической группы (XR-117) отвечает на вопрос «что это
                // за правило» лучше счётчика, поэтому счётчик уходит во вторую
                // строку. Пресеты без имён показываются как раньше.
                Column {
                    Text(
                        rule.name.ifBlank { summary.ifBlank { emptyRuleLabel } },
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    if (rule.name.isNotBlank() && summary.isNotBlank()) {
                        Text(
                            summary,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
            if (expanded) {
                Spacer(Modifier.height(8.dp))
                Text(
                    (rule.domains + rule.ipRanges + rule.geoip).joinToString("\n"),
                    fontFamily = FontFamily.Monospace,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

// ── Точка входа со вкладки «Серверы» ────────────────────────────────

/** Строка-секция на «Серверах»: сводка и переход в редактор. */
@Composable
fun RulesEntryCard(
    userRulesCount: Int,
    presetName: String,
    onClick: () -> Unit,
) {
    Text(
        stringResource(R.string.rules_entry_title),
        style = MaterialTheme.typography.titleMedium,
        modifier = Modifier.padding(vertical = 8.dp),
    )
    OutlinedCard(modifier = Modifier.fillMaxWidth().clickable(onClick = onClick)) {
        Row(
            modifier = Modifier.padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                Icons.AutoMirrored.Filled.Rule,
                null,
                tint = MaterialTheme.colorScheme.primary,
            )
            Spacer(Modifier.width(12.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(stringResource(R.string.rules_title), style = MaterialTheme.typography.bodyLarge)
                Text(
                    if (presetName.isNotBlank()) {
                        stringResource(R.string.rules_entry_summary_with_preset, userRulesCount, presetName)
                    } else {
                        stringResource(R.string.rules_entry_summary, userRulesCount)
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
    Spacer(Modifier.height(16.dp))
}
