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
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.xrproxy.app.data.CachedPreset
import com.xrproxy.app.data.UserRule
import com.xrproxy.app.data.UserRulesStore
import com.xrproxy.app.ui.PresetRefresh
import com.xrproxy.app.ui.VpnViewModel
import com.xrproxy.app.ui.components.XrSnackbarHost
import com.xrproxy.app.ui.UiSeverity
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

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

    // Единая точка сохранения: подсказка «применятся при следующем
    // подключении» показывается только при живом туннеле, когда она несёт
    // информацию (LLD-05 §3.10); в Idle список сам себе подтверждение.
    fun applyRules(newRules: List<UserRule>) {
        viewModel.saveUserRules(newRules)
        if (state.connected) {
            snack("Правила сохранены. Применятся при следующем подключении")
        }
    }

    val presetSnapshot = preset
    if (detailsOpen && presetSnapshot != null) {
        PresetDetailsScreen(preset = presetSnapshot, onBack = { detailsOpen = false })
        return
    }

    Scaffold(
        snackbarHost = { XrSnackbarHost(snackbarHostState, lastSeverity) },
        topBar = {
            TopAppBar(
                title = { Text("Правила маршрутизации") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, "Назад")
                    }
                },
                actions = {
                    IconButton(onClick = { tomlOpen = true }) {
                        Icon(Icons.Default.Code, "Показать TOML")
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
                PresetCard(
                    presetName = presetName,
                    preset = preset,
                    refreshing = refreshing,
                    onRefresh = {
                        refreshing = true
                        scope.launch {
                            when (val r = viewModel.refreshPresetNow()) {
                                is PresetRefresh.Updated ->
                                    snack("Пресет обновлён до v${r.version}")
                                is PresetRefresh.UpToDate ->
                                    snack("Пресет v${r.version} актуален")
                                is PresetRefresh.Failed ->
                                    snack(r.message, UiSeverity.Error)
                            }
                            presetEpoch++
                            refreshing = false
                        }
                    },
                    onDetails = { detailsOpen = true },
                )
            }
            item {
                Text(
                    "Мои правила",
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.padding(top = 16.dp, bottom = 4.dp),
                )
                Text(
                    "Срабатывают раньше пресета, выше в списке — раньше.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
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
                OutlinedButton(
                    onClick = {
                        if (rules.size >= UserRulesStore.MAX_RULES) {
                            snack("Достигнут лимит ${UserRulesStore.MAX_RULES} правил", UiSeverity.Warn)
                        } else {
                            addDialogOpen = true
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Icon(Icons.Default.Add, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Добавить правило")
                }
                Spacer(Modifier.height(24.dp))
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
    if (tomlOpen) {
        TomlPreviewDialog(
            toml = buildMergedToml(rules, "direct", preset),
            onDismiss = { tomlOpen = false },
            onCopied = { snack("Скопировано") },
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

// ── Карточка пресета ────────────────────────────────────────────────

@Composable
private fun PresetCard(
    presetName: String,
    preset: CachedPreset?,
    refreshing: Boolean,
    onRefresh: () -> Unit,
    onDetails: () -> Unit,
) {
    OutlinedCard(modifier = Modifier.fillMaxWidth().padding(top = 16.dp)) {
        Column(modifier = Modifier.padding(16.dp)) {
            if (presetName.isBlank()) {
                Text("Пресет не подключён", style = MaterialTheme.typography.titleMedium)
                Spacer(Modifier.height(4.dp))
                Text(
                    "Правила сервера раздаёт хаб. Примените приглашение или " +
                        "укажите хаб в настройках сервера.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                return@Column
            }
            Text(
                if (preset != null) "Пресет $presetName · v${preset.version}"
                else "Пресет $presetName",
                style = MaterialTheme.typography.titleMedium,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                if (preset != null) {
                    val date = preset.updatedAt.take(10)
                    "${preset.rules.size} правил" +
                        (if (date.isNotBlank()) " · обновлён $date" else "")
                } else {
                    "Ещё не скачан с хаба"
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
                    Text("Обновить сейчас")
                }
                OutlinedButton(onClick = onDetails, enabled = preset != null) {
                    Text("Подробнее")
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
            Icon(Icons.Default.MoreVert, "Меню правила")
        }
        DropdownMenu(expanded = menuExpanded, onDismissRequest = { menuExpanded = false }) {
            DropdownMenuItem(
                text = { Text("Изменить") },
                onClick = { menuExpanded = false; onEdit() },
            )
            DropdownMenuItem(
                text = { Text("Выше") },
                enabled = !isFirst,
                onClick = { menuExpanded = false; onMove(-1) },
            )
            DropdownMenuItem(
                text = { Text("Ниже") },
                enabled = !isLast,
                onClick = { menuExpanded = false; onMove(1) },
            )
            DropdownMenuItem(
                text = { Text("В начало") },
                enabled = !isFirst,
                onClick = { menuExpanded = false; onMove(Int.MIN_VALUE / 2) },
            )
            DropdownMenuItem(
                text = { Text("В конец") },
                enabled = !isLast,
                onClick = { menuExpanded = false; onMove(Int.MAX_VALUE / 2) },
            )
            DropdownMenuItem(
                text = { Text("Удалить", color = MaterialTheme.colorScheme.error) },
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
                title = { Text("${preset.name} · v${preset.version}") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, "Назад")
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
                    "Пресет раздаёт хаб, на устройстве он только читается. " +
                        "Переопределить домен можно своим правилом сверху.",
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
                val summary = buildList {
                    if (rule.domains.isNotEmpty()) add("доменов: ${rule.domains.size}")
                    if (rule.ipRanges.isNotEmpty()) add("IP: ${rule.ipRanges.size}")
                    if (rule.geoip.isNotEmpty()) add("geoip: ${rule.geoip.size}")
                }.joinToString(" · ")
                Text(
                    summary.ifBlank { "пустое правило" },
                    style = MaterialTheme.typography.bodyMedium,
                )
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
        "Маршрутизация",
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
                Text("Правила маршрутизации", style = MaterialTheme.typography.bodyLarge)
                Text(
                    buildString {
                        append("Мои правила: $userRulesCount")
                        if (presetName.isNotBlank()) append(" · пресет $presetName")
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
    Spacer(Modifier.height(16.dp))
}
