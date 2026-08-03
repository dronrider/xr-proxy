@file:OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)

package com.xrproxy.app.ui.files

import android.content.Context
import android.content.Intent
import android.os.Build
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.AddLink
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Remove
import androidx.compose.material.icons.filled.Replay
import androidx.compose.material.icons.filled.SaveAlt
import androidx.compose.material.icons.filled.Schedule
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TriStateCheckbox
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.state.ToggleableState
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import androidx.lifecycle.viewmodel.compose.viewModel
import com.xrproxy.app.data.StorageAccess
import com.xrproxy.app.ui.components.XrPullToRefresh
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.TreeNode
import com.xrproxy.app.model.explorerLevel
import kotlinx.coroutines.launch
import java.io.File

/**
 * Files tab (LLD-19, XR-031): a list of shares ("drives") and an Explorer that
 * navigates one share's folders. One control per file row (XR-044): the plus
 * queues a download, the running row shows progress with a cancel, the minus
 * removes the local copy, a broken download keeps its progress under a red tint
 * with a retry. The row tap only opens a downloaded file. Folders are tri-state
 * like selective sync in Drive/Dropbox.
 */
@Composable
fun FilesScreen(hubUrl: String?, inviteToken: String?, modifier: Modifier = Modifier) {
    val vm: FilesViewModel = viewModel()
    val ui by vm.ui.collectAsState()
    val configs by vm.configs.collectAsState()
    val context = LocalContext.current

    // Storage-directory picker (XR-043). A custom folder needs all-files access;
    // we route the user to the system settings to grant it, then to the folder
    // picker, and hand the engine the picked folder's real path.
    var pickShareId by rememberSaveable { mutableStateOf<String?>(null) }
    val treePicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        val sid = pickShareId
        pickShareId = null
        if (sid == null) return@rememberLauncherForActivityResult
        if (uri == null) {
            vm.dismissStorageDialog()
            return@rememberLauncherForActivityResult
        }
        val path = StorageAccess.treeUriToRealPath(uri)
        if (path == null) {
            Toast.makeText(context, "Выберите папку на основном хранилище (не SD-карту)", Toast.LENGTH_LONG).show()
            vm.dismissStorageDialog()
        } else {
            vm.chooseStorage(sid, path)
        }
    }
    val grantLauncher = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) {
        if (StorageAccess.hasAllFilesAccess()) {
            treePicker.launch(null)
        } else {
            pickShareId = null
            vm.dismissStorageDialog()
            Toast.makeText(context, "Доступ ко всем файлам не выдан", Toast.LENGTH_LONG).show()
        }
    }
    val startCustomPick: (String) -> Unit = startCustomPick@{ sid ->
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
            Toast.makeText(context, "Своя папка доступна на Android 11+", Toast.LENGTH_LONG).show()
            return@startCustomPick
        }
        pickShareId = sid
        if (StorageAccess.hasAllFilesAccess()) treePicker.launch(null)
        else grantLauncher.launch(StorageAccess.allFilesAccessSettings(context))
    }

    LaunchedEffect(Unit) {
        vm.refreshHub(hubUrl, inviteToken)
        vm.syncAllNow()
    }
    LaunchedEffect(ui.message) {
        ui.message?.let {
            Toast.makeText(context, it, Toast.LENGTH_SHORT).show()
            vm.consumeMessage()
        }
    }
    LaunchedEffect(ui.openFileEvent) {
        ui.openFileEvent?.let {
            openLocalFile(context, it)
            vm.consumeOpenEvent()
        }
    }

    val openConfig = configs.firstOrNull { it.shareId == ui.openShareId }
    BackHandler(enabled = openConfig != null) { vm.navigateUp() }

    // Удаление шары подтверждается не диалогом, а undo-снекбаром (макет 1d):
    // конфиг придерживаем до истечения снекбара, «Отменить» кладёт его назад
    // как был. Локальные файлы удаление не трогает, восстановление полное.
    val snackbarHost = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    val deleteWithUndo: (ShareConfig) -> Unit = { cfg ->
        vm.removeShare(cfg.shareId)
        scope.launch {
            // Второе удаление подряд вытесняет висящий снекбар: очередь
            // showSnackbar задержала бы отклик на десять секунд.
            snackbarHost.currentSnackbarData?.dismiss()
            val result = snackbarHost.showSnackbar(
                message = "Шара «${cfg.name}» удалена",
                actionLabel = "Отменить",
                duration = SnackbarDuration.Long,
            )
            if (result == SnackbarResult.ActionPerformed) vm.restoreShare(cfg)
        }
    }

    Box(modifier = modifier) {
        if (openConfig != null) {
            ExplorerView(vm, ui, openConfig, context, Modifier)
        } else {
            ShareListView(vm, ui, configs, hubUrl, inviteToken, deleteWithUndo, Modifier)
        }
        SnackbarHost(
            snackbarHost,
            modifier = Modifier.align(Alignment.BottomCenter),
        )
    }

    val storageCfg = configs.firstOrNull { it.shareId == ui.storageDialogFor }
    if (storageCfg != null) {
        StorageDialog(
            cfg = storageCfg,
            promptMode = ui.storagePromptMode,
            onAppDir = { vm.chooseStorage(storageCfg.shareId, null) },
            onCustom = { vm.hideStorageDialog(); startCustomPick(storageCfg.shareId) },
            onDismiss = { vm.dismissStorageDialog() },
        )
    }

    // Диалог живёт на верхнем уровне экрана, а не внутри проводника: импорт
    // доезжает и после того, как папку закрыли, а тост с длинным текстом
    // причину всё равно съедал.
    ui.importError?.let { text ->
        ImportErrorDialog(
            text = text,
            onCopy = {
                Toast.makeText(context, "Скопировано", Toast.LENGTH_SHORT).show()
            },
            onDismiss = { vm.dismissImportError() },
        )
    }
}

// ── Storage-directory dialog (XR-043) ───────────────────────────────

@Composable
private fun StorageDialog(
    cfg: ShareConfig,
    promptMode: Boolean,
    onAppDir: () -> Unit,
    onCustom: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (promptMode) "Куда сохранять файлы?" else "Папка хранения") },
        text = {
            Column {
                if (promptMode) {
                    Text(
                        "Куда складывать скачанные файлы шары «${cfg.name}». Поменять можно позже.",
                        fontSize = 13.sp,
                    )
                } else {
                    Text("Сейчас: ${StorageAccess.label(cfg.storagePath)}", fontSize = 13.sp)
                    Spacer(Modifier.height(4.dp))
                    Text(
                        "Смена папки перенесёт уже скачанное в новое место без повторной загрузки.",
                        fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                if (!StorageAccess.customFolderSupported()) {
                    Spacer(Modifier.height(6.dp))
                    Text(
                        "Своя папка доступна на Android 11+.",
                        fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = onCustom, enabled = StorageAccess.customFolderSupported()) {
                Text("Своя папка…")
            }
        },
        dismissButton = { TextButton(onClick = onAppDir) { Text("Папка приложения") } },
    )
}

// ── Share list (the "drives") ───────────────────────────────────────

@Composable
private fun ShareListView(
    vm: FilesViewModel,
    ui: FilesViewModel.UiState,
    configs: List<ShareConfig>,
    hubUrl: String?,
    inviteToken: String?,
    onDeleteShare: (ShareConfig) -> Unit,
    modifier: Modifier,
) {
    val knownIds = configs.map { it.shareId }.toSet()
    val addable = ui.hubShares.filter { it.shareId !in knownIds }
    // Какой шаре открыт лист действий (кнопка с тремя точками). Держим id, а
    // не конфиг: тумблер синка в листе должен видеть живое состояние из store,
    // не снимок.
    var menuShareId by remember { mutableStateOf<String?>(null) }

    // Свайп-вниз обновляет список по инвайту (XR-125, стандарт XR-181): тот же
    // refreshHub, что и кнопка в шапке. Вход на вкладку тоже дёргает refreshHub,
    // поэтому индикатор жеста вешаем не на общий loadingHub, а на локальный
    // флаг, который взводят только жест и кнопка (XR-232): выезжающий сам по
    // себе спиннер выглядел поломкой.
    var manualRefresh by remember { mutableStateOf(false) }
    LaunchedEffect(ui.loadingHub) { if (!ui.loadingHub) manualRefresh = false }
    val refreshByHand = { manualRefresh = true; vm.refreshHub(hubUrl, inviteToken) }
    XrPullToRefresh(
        refreshing = manualRefresh && ui.loadingHub,
        onRefresh = refreshByHand,
        modifier = modifier,
    ) {
    LazyColumn(
        modifier = Modifier.fillMaxSize().padding(horizontal = 16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        item {
            Row(
                modifier = Modifier.fillMaxWidth().padding(top = 12.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Файлы", style = MaterialTheme.typography.titleLarge)
                IconButton(onClick = refreshByHand) {
                    Icon(Icons.Default.Refresh, contentDescription = "Обновить по инвайту")
                }
            }
        }
        if (ui.hubOffline) {
            item {
                Text(
                    "Хаб недоступен, показан сохранённый список",
                    fontSize = 11.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        if (ui.migratingShareId != null) item { ProgressBar(ui.transfer) { vm.cancelTransfer() } }

        if (addable.isNotEmpty()) {
            item { SectionLabel("Доступно по инвайту") }
            items(addable, key = { it.shareId }) { g ->
                Card(modifier = Modifier.fillMaxWidth()) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(12.dp),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(g.name, modifier = Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                        Button(onClick = { vm.addShare(g) }) { Text("Добавить") }
                    }
                }
            }
        }

        item { SectionLabel("Мои шары") }
        // Until the store has loaded, an empty list means "still opening", so
        // hold the empty-state text back instead of flashing it.
        if (configs.isEmpty() && ui.storeReady) {
            item {
                Text(
                    if (ui.hubOffline) "Нет сети, а сохранённых шар пока нет"
                    else "Пока нет шар. Обнови список и добавь нужные.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(8.dp),
                )
            }
        }
        // Карточка по макету 2a: имя одной строкой во всю ширину (эллипсис в
        // середине держит начало и хвост различимыми), под ним строка статуса
        // с точкой. Тумблер, папка и удаление переехали в лист действий по
        // кнопке с тремя точками (макет 1d), единственное касание карточки
        // открывает шару (XR-055).
        items(configs, key = { it.shareId }) { cfg ->
            Card(
                modifier = Modifier.fillMaxWidth().clickable { vm.openShare(cfg) },
                colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(start = 12.dp, top = 12.dp, bottom = 12.dp, end = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    ShareIconTile(cfg.syncEnabled)
                    Spacer(Modifier.width(12.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Text(cfg.name, style = MaterialTheme.typography.titleMedium, maxLines = 1,
                            overflow = TextOverflow.MiddleEllipsis)
                        Spacer(Modifier.height(4.dp))
                        ShareStatusLine(cfg)
                    }
                    IconButton(onClick = { menuShareId = cfg.shareId }) {
                        Icon(Icons.Default.MoreVert, contentDescription = "Действия с шарой")
                    }
                }
            }
        }
        item { Spacer(Modifier.height(24.dp)) }
    }
    }

    val menuCfg = configs.firstOrNull { it.shareId == menuShareId }
    if (menuCfg != null) {
        ShareActionsSheet(
            cfg = menuCfg,
            vm = vm,
            onDelete = onDeleteShare,
            onDismiss = { menuShareId = null },
        )
    }
}

/** Ведущая плитка карточки: папка в скруглённом тонированном квадрате, тон
 *  следует за состоянием синка, как и точка статуса. */
@Composable
private fun ShareIconTile(syncEnabled: Boolean) {
    val tint = if (syncEnabled) MaterialTheme.colorScheme.primary
    else MaterialTheme.colorScheme.onSurfaceVariant
    Box(
        modifier = Modifier.size(40.dp)
            .background(tint.copy(alpha = 0.14f), RoundedCornerShape(12.dp)),
        contentAlignment = Alignment.Center,
    ) {
        Icon(Icons.Default.Folder, contentDescription = null, tint = tint,
            modifier = Modifier.size(20.dp))
    }
}

/** Строка статуса из макета 2a: точка-индикатор плюс текст. Счётчик держим
 *  прежний («выбрано: N»): в выборе бывают и папки, «N файлов» приврал бы. */
@Composable
private fun ShareStatusLine(cfg: ShareConfig) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(
            modifier = Modifier.size(8.dp).background(
                if (cfg.syncEnabled) MaterialTheme.colorScheme.primary
                else MaterialTheme.colorScheme.outline,
                CircleShape,
            ),
        )
        Spacer(Modifier.width(8.dp))
        Text(
            shareStatusText(cfg),
            fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1, overflow = TextOverflow.Ellipsis,
        )
    }
}

private fun shareStatusText(cfg: ShareConfig): String {
    val selection = if (cfg.selection.isEmpty()) "ничего не выбрано" else "выбрано: ${cfg.selection.size}"
    return if (cfg.syncEnabled) "Синхронизируется \u00B7 $selection"
    else "Синхронизация выключена \u00B7 $selection"
}

// -- Лист действий шары (макет 1d) ----------------------------------

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ShareActionsSheet(
    cfg: ShareConfig,
    vm: FilesViewModel,
    onDelete: (ShareConfig) -> Unit,
    onDismiss: () -> Unit,
) {
    val sheetState = rememberModalBottomSheetState()
    val scope = rememberCoroutineScope()
    // Кнопки закрывают лист той же анимацией, что свайп и тап по скриму:
    // сначала hide, действие после, иначе лист исчезает рывком.
    fun dismissThen(action: () -> Unit) {
        scope.launch { sheetState.hide() }.invokeOnCompletion {
            onDismiss()
            action()
        }
    }
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = sheetState) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ShareIconTile(cfg.syncEnabled)
            Spacer(Modifier.width(12.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(cfg.name, style = MaterialTheme.typography.titleMedium, maxLines = 1,
                    overflow = TextOverflow.MiddleEllipsis)
                Spacer(Modifier.height(2.dp))
                Text(
                    shareStatusText(cfg),
                    fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1, overflow = TextOverflow.Ellipsis,
                )
            }
        }
        HorizontalDivider(modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
        SheetActionRow(
            icon = { Icon(Icons.Default.FolderOpen, contentDescription = null) },
            title = "Открыть шару",
            chevron = true,
            onClick = { dismissThen { vm.openShare(cfg) } },
        )
        SheetActionRow(
            icon = { Icon(Icons.Default.Sync, contentDescription = null) },
            title = "Синхронизация",
            trailing = {
                Switch(
                    checked = cfg.syncEnabled,
                    onCheckedChange = { vm.setSyncEnabled(cfg.shareId, it) },
                )
            },
        )
        SheetActionRow(
            icon = { Icon(Icons.Default.SaveAlt, contentDescription = null) },
            title = "Папка на устройстве",
            subtitle = StorageAccess.label(cfg.storagePath),
            chevron = true,
            onClick = { dismissThen { vm.openStorageDialog(cfg.shareId) } },
        )
        HorizontalDivider(modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
        SheetActionRow(
            icon = {
                Icon(Icons.Default.Delete, contentDescription = null,
                    tint = MaterialTheme.colorScheme.error)
            },
            title = "Удалить шару",
            titleColor = MaterialTheme.colorScheme.error,
            onClick = { dismissThen { onDelete(cfg) } },
        )
        Spacer(Modifier.height(16.dp))
    }
}

@Composable
private fun SheetActionRow(
    icon: @Composable () -> Unit,
    title: String,
    subtitle: String? = null,
    titleColor: Color = Color.Unspecified,
    chevron: Boolean = false,
    trailing: (@Composable () -> Unit)? = null,
    onClick: (() -> Unit)? = null,
) {
    Row(
        modifier = Modifier.fillMaxWidth()
            .let { if (onClick != null) it.clickable(onClick = onClick) else it }
            .padding(horizontal = 16.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        icon()
        Spacer(Modifier.width(16.dp))
        Column(modifier = Modifier.weight(1f)) {
            Text(title, color = titleColor, style = MaterialTheme.typography.bodyLarge)
            if (subtitle != null) {
                Text(
                    subtitle,
                    fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1, overflow = TextOverflow.Ellipsis,
                )
            }
        }
        if (trailing != null) trailing()
        else if (chevron) {
            Icon(
                Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

// ── Explorer (one share's folders) ──────────────────────────────────

@Composable
private fun ExplorerView(
    vm: FilesViewModel,
    ui: FilesViewModel.UiState,
    cfg: ShareConfig,
    context: Context,
    modifier: Modifier,
) {
    var detailsFor by remember { mutableStateOf<ManifestEntry?>(null) }
    // Открытие шары поднимает manifestLoading само, индикатор жеста поэтому
    // держим на локальном флаге ручных обновлений, как в списке шар (XR-232).
    var manualRefresh by remember { mutableStateOf(false) }
    LaunchedEffect(ui.manifestLoading) { if (!ui.manifestLoading) manualRefresh = false }
    val refreshByHand = { manualRefresh = true; vm.refreshManifest(cfg) }
    // Derived once per state change, not per recomposition: a big manifest
    // with a long queue would otherwise be rescanned for every visible row on
    // every 500ms progress tick.
    val level = remember(ui.manifest, ui.currentPath) { explorerLevel(ui.manifest, ui.currentPath) }
    val queuedPaths = remember(ui.queue, cfg.shareId) {
        ui.queue.asSequence().filter { it.shareId == cfg.shareId }.map { it.entry.path }.toHashSet()
    }
    val headPath = ui.queue.firstOrNull()?.takeIf { it.shareId == cfg.shareId }?.entry?.path
    val failedByPath = remember(ui.failed, cfg.shareId) {
        ui.failed.filter { it.shareId == cfg.shareId }.associateBy { it.path }
    }
    val folderPresence = remember(ui.manifest, ui.localPaths, queuedPaths, ui.currentPath) {
        folderPresence(ui.manifest, ui.currentPath, ui.localPaths, queuedPaths)
    }

    Column(modifier = modifier.fillMaxSize().padding(horizontal = 12.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(top = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TextButton(
                onClick = { vm.navigateUp() },
                contentPadding = PaddingValues(horizontal = 8.dp),
            ) { Text("‹ Назад") }
            Spacer(Modifier.weight(1f))
            // URL import (LLD-29): the agent downloads the page into the open
            // folder. Shown only when the grant carries share:import.
            if (cfg.canImport) {
                IconButton(onClick = { vm.openImportDialog(cfg.shareId) }) {
                    Icon(Icons.Default.AddLink, contentDescription = "Импорт по URL")
                }
            }
            // Refresh the listing from the agent. Deliberately not the sync
            // action: the old circular-arrows button confused both meanings
            // (XR-044), downloads now go through the per-row controls.
            IconButton(onClick = refreshByHand) {
                Icon(Icons.Default.Refresh, contentDescription = "Обновить список")
            }
            Spacer(Modifier.width(6.dp))
            Text("Синк", fontSize = 12.sp)
            Spacer(Modifier.width(4.dp))
            Switch(checked = cfg.syncEnabled, onCheckedChange = { vm.setSyncEnabled(cfg.shareId, it) })
        }
        Breadcrumbs(cfg.name, ui.currentPath) { vm.navigateTo(it) }
        if (ui.offlineLocal && ui.manifest.isNotEmpty()) {
            Text(
                // Полный кэшированный манифест показывает и не скачанные файлы,
                // так что «только скачанные» тут врало бы (XR-099).
                if (ui.offlineFullListing) "Офлайн: показан последний известный список, файлы могли измениться"
                else "Офлайн: показаны только скачанные файлы",
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(vertical = 2.dp),
            )
        }
        if (ui.migratingShareId != null) ProgressBar(ui.transfer) { vm.cancelTransfer() }
        // The live import job's row (LLD-29): the agent downloads, this is just
        // the counter and the cancel; leaving the screen does not interrupt.
        val importJob = ui.importJob
        if (importJob != null && importJob.shareId == cfg.shareId) {
            ImportRow(importJob) { vm.cancelImport(cfg) }
        }
        HorizontalDivider(modifier = Modifier.padding(vertical = 4.dp))

        // Свайп-вниз перезапрашивает манифест у агента (стандарт XR-181): тот же
        // refreshManifest, что и кнопка в шапке. Контент всегда LazyColumn, даже
        // пустой, иначе жесту не за что зацепиться; индикатор рисует сам жест,
        // отдельный спиннер убран.
        XrPullToRefresh(
            refreshing = manualRefresh && ui.manifestLoading,
            onRefresh = refreshByHand,
            modifier = Modifier.weight(1f),
        ) {
            LazyColumn(modifier = Modifier.fillMaxSize()) {
                when {
                    ui.manifest.isEmpty() && ui.offlineLocal -> item {
                        Text(
                            "Нет сети, а скачанных файлов пока нет", modifier = Modifier.padding(16.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    // Первое открытие шары без локального кэша: список ещё пуст,
                    // маленького индикатора жеста мало, показываем явный спиннер.
                    level.isEmpty() && ui.manifestLoading -> item {
                        CircularProgressIndicator(modifier = Modifier.padding(16.dp))
                    }
                    level.isEmpty() && !ui.manifestLoading -> item {
                        Text(
                            "Папка пуста", modifier = Modifier.padding(16.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    else -> {
                        items(level, key = { it.path }) { node ->
                            when (node) {
                                is TreeNode.Folder -> FolderRow(node, folderPresence[node.path], cfg, vm)
                                is TreeNode.FileNode -> FileRow(
                                    node, cfg, ui, vm,
                                    isHead = node.entry.path == headPath,
                                    queued = node.entry.path != headPath && node.entry.path in queuedPaths,
                                    failed = failedByPath[node.entry.path],
                                ) { detailsFor = it }
                            }
                            HorizontalDivider()
                        }
                        item { Spacer(Modifier.height(24.dp)) }
                    }
                }
            }
        }
    }

    if (ui.importDialogFor == cfg.shareId) {
        ImportDialog(
            onStart = { url, height -> vm.startImport(cfg, url, height) },
            onDismiss = { vm.dismissImportDialog() },
        )
    }

    detailsFor?.let { e ->
        AlertDialog(
            onDismissRequest = { detailsFor = null },
            confirmButton = { TextButton(onClick = { detailsFor = null }) { Text("Закрыть") } },
            title = { Text("Файл") },
            text = {
                Column {
                    Text(e.path.substringAfterLast('/'), style = MaterialTheme.typography.titleSmall)
                    Spacer(Modifier.height(6.dp))
                    Text("Путь: ${e.path}", fontSize = 12.sp)
                    Text("Размер: ${humanSize(e.size)}", fontSize = 12.sp)
                    Text("SHA-256: ${e.sha256.take(16)}…", fontSize = 12.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
        )
    }
}

// -- URL import (LLD-29, UI texts fixed in п. 2.8) ------------------

/** The import dialog: a link field and a row of quality chips. Quality is a
 *  top-down wish: "Максимум" sends no height, so only the owner's cap
 *  limits the download. */
@Composable
private fun ImportDialog(onStart: (String, Int?) -> Unit, onDismiss: () -> Unit) {
    var url by remember { mutableStateOf("") }
    // null means "Максимум"; the chips are fixed since the phone does not
    // know the owner's cap.
    var height by remember { mutableStateOf<Int?>(1080) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Импорт по URL") },
        text = {
            Column {
                OutlinedTextField(
                    value = url,
                    onValueChange = { url = it },
                    label = { Text("Ссылка") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(8.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    FilterChip(
                        selected = height == 720,
                        onClick = { height = 720 },
                        label = { Text("720p") },
                    )
                    FilterChip(
                        selected = height == 1080,
                        onClick = { height = 1080 },
                        label = { Text("1080p") },
                    )
                    FilterChip(
                        selected = height == null,
                        onClick = { height = null },
                        label = { Text("Максимум") },
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = { onStart(url, height) },
                enabled = url.isNotBlank(),
            ) { Text("Импортировать") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Отмена") } },
    )
}

/**
 * Причина сорвавшегося импорта целиком (XR-161). Агент присылает хвост stderr
 * плагина, а это несколько строк с адресами и кодами: в тосте от него видно
 * два обрывка, и те исчезают через секунду. Текст моноширинный (это выхлоп
 * утилиты), скроллится и уходит в буфер целиком, чтобы его можно было
 * переслать владельцу шары.
 */
@Composable
private fun ImportErrorDialog(text: String, onCopy: () -> Unit, onDismiss: () -> Unit) {
    val clipboard = LocalClipboardManager.current
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Импорт не удался") },
        text = {
            Text(
                text,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
                lineHeight = 16.sp,
                modifier = Modifier.fillMaxWidth().verticalScroll(rememberScrollState()),
            )
        },
        confirmButton = {
            TextButton(onClick = {
                clipboard.setText(AnnotatedString(text))
                onCopy()
            }) { Text("Скопировать") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Закрыть") } },
    )
}

/** The task row above the file list: "Импорт: N%" with a cancel cross. */
@Composable
private fun ImportRow(job: FilesViewModel.ImportJob, onCancel: () -> Unit) {
    Card(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Row(
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                job.progress?.let { "Импорт: ${it.toInt()}%" } ?: "Импорт...",
                fontSize = 13.sp,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onCancel) {
                Icon(Icons.Default.Close, contentDescription = "Отменить импорт")
            }
        }
    }
}

/** A folder row (XR-044): tri-state like selective-sync folders in Drive or
 *  Dropbox. Off and indeterminate taps queue whatever is missing under the
 *  folder; the On tap unselects the subtree and removes its local copies. */
@Composable
private fun FolderRow(
    node: TreeNode.Folder,
    presence: FolderPresence?,
    cfg: ShareConfig,
    vm: FilesViewModel,
) {
    val state = when {
        presence == null || presence.present == 0 -> ToggleableState.Off
        presence.present == presence.total -> ToggleableState.On
        else -> ToggleableState.Indeterminate
    }
    Row(
        modifier = Modifier.fillMaxWidth().clickable { vm.navigateTo(node.path) }.padding(vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        TriStateCheckbox(
            state = state,
            onClick = {
                if (state == ToggleableState.On) vm.removeFolder(cfg, node.path)
                else vm.downloadFolder(cfg, node.path)
            },
        )
        Icon(Icons.Default.Folder, contentDescription = null, modifier = Modifier.size(24.dp))
        Spacer(Modifier.width(8.dp))
        Column(modifier = Modifier.weight(1f)) {
            Text(node.name, maxLines = 1, fontSize = 14.sp, overflow = TextOverflow.MiddleEllipsis)
            Text("${node.fileCount} файл(ов)", fontSize = 10.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Text(">", fontSize = 22.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(6.dp))
    }
}

/** One file row (XR-044). The trailing control always carries an action for the
 *  current state: plus = queue the download, cross = cancel it, minus = delete
 *  the local copy, replay = resume a broken download from its partial. The row
 *  tap only opens a downloaded file; progress (ours or the background mirror's,
 *  matched by share + path) is painted behind the row itself. */
@Composable
private fun FileRow(
    node: TreeNode.FileNode,
    cfg: ShareConfig,
    ui: FilesViewModel.UiState,
    vm: FilesViewModel,
    isHead: Boolean,
    queued: Boolean,
    failed: FilesViewModel.FailedDownload?,
    onDetails: (ManifestEntry) -> Unit,
) {
    val path = node.entry.path
    val downloaded = ui.localPaths.contains(path)
    // The native transfer snapshot claimed by this row (ours or the mirror's).
    val snap = ui.transfer?.takeIf { !downloaded && it.share == cfg.shareId && it.file == path }
    val transferring = snap != null
    // Transferring but neither ours nor queued: the background mirror fetches it.
    val bgFetch = transferring && !isHead && !queued

    val errorColor = MaterialTheme.colorScheme.error
    val primary = MaterialTheme.colorScheme.primary
    // A multi-file mirror pass reports aggregate bytes, a per-row fraction is
    // only honest for a single-file transfer.
    val fillFrac = when {
        snap != null && snap.filesTotal == 1L && snap.bytesTotal > 0 ->
            (snap.bytesDone.toFloat() / snap.bytesTotal).coerceIn(0f, 1f)
        snap == null && !queued && failed != null && failed.bytesTotal > 0 ->
            (failed.bytesDone.toFloat() / failed.bytesTotal).coerceIn(0f, 1f)
        else -> 0f
    }
    val showError = failed != null && !transferring && !queued && !downloaded

    Row(
        modifier = Modifier.fillMaxWidth()
            .drawBehind {
                if (showError) {
                    drawRect(errorColor.copy(alpha = 0.10f))
                    if (fillFrac > 0f) {
                        drawRect(errorColor.copy(alpha = 0.25f), size = size.copy(width = size.width * fillFrac))
                    }
                } else if (transferring && fillFrac > 0f) {
                    drawRect(primary.copy(alpha = 0.15f), size = size.copy(width = size.width * fillFrac))
                }
            }
            .combinedClickable(
                onClick = { if (downloaded) vm.openLocal(cfg, node.entry) },
                onLongClick = { onDetails(node.entry) },
            )
            .padding(vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(modifier = Modifier.weight(1f).padding(start = 14.dp)) {
            Text(node.name, maxLines = 1, fontSize = 13.sp, overflow = TextOverflow.MiddleEllipsis)
            Text(
                when {
                    downloaded -> humanSize(node.entry.size) + " - скачано, тап откроет"
                    snap != null && snap.filesTotal == 1L ->
                        "${humanSize(snap.bytesDone)} из ${humanSize(node.entry.size)}" +
                            " - ${humanSize(snap.speedBytesPerSec)}/с"
                    bgFetch -> "качается фоновым синком"
                    isHead -> "готовится..."
                    queued -> humanSize(node.entry.size) + " - в очереди"
                    failed != null -> "оборвалось на ${humanSize(failed.bytesDone)} из ${humanSize(failed.bytesTotal)}"
                    else -> humanSize(node.entry.size)
                },
                fontSize = 10.sp,
                color = when {
                    showError -> errorColor
                    downloaded -> primary
                    else -> MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
        }
        when {
            downloaded -> IconButton(onClick = { vm.removeLocal(cfg, node.entry) }) {
                Icon(Icons.Default.Remove, contentDescription = "Удалить с устройства")
            }
            isHead || bgFetch -> IconButton(onClick = { vm.cancelDownload(cfg.shareId, path) }) {
                Icon(Icons.Default.Close, contentDescription = "Отменить загрузку")
            }
            queued -> IconButton(onClick = { vm.cancelDownload(cfg.shareId, path) }) {
                Icon(Icons.Default.Schedule, contentDescription = "Убрать из очереди")
            }
            failed != null -> IconButton(onClick = { vm.enqueue(cfg, node.entry) }) {
                Icon(Icons.Default.Replay, contentDescription = "Докачать", tint = errorColor)
            }
            else -> IconButton(onClick = { vm.enqueue(cfg, node.entry) }) {
                Icon(Icons.Default.Add, contentDescription = "Скачать", tint = DownloadGreen)
            }
        }
    }
}

/** How much of a folder's subtree is on the device or queued, per sub-folder of
 *  the open level. One pass over the manifest instead of a rescan per row. */
private data class FolderPresence(val total: Int, val present: Int)

private fun folderPresence(
    manifest: List<ManifestEntry>,
    dir: String,
    localPaths: Set<String>,
    queuedPaths: Set<String>,
): Map<String, FolderPresence> {
    val prefix = if (dir.isEmpty()) "" else "$dir/"
    val acc = HashMap<String, IntArray>()
    for (e in manifest) {
        if (!e.path.startsWith(prefix)) continue
        val rest = e.path.substring(prefix.length)
        val slash = rest.indexOf('/')
        if (slash < 0) continue
        val folder = if (dir.isEmpty()) rest.substring(0, slash) else "$dir/${rest.substring(0, slash)}"
        val a = acc.getOrPut(folder) { IntArray(2) }
        a[0]++
        if (e.path in localPaths || e.path in queuedPaths) a[1]++
    }
    return acc.mapValues { FolderPresence(it.value[0], it.value[1]) }
}

/** The storage-migration card. [p] null means the native side has not flipped
 *  active yet (still listing files), rendered as an indeterminate start. */
@Composable
private fun ProgressBar(p: FilesViewModel.Progress?, onCancel: () -> Unit) {
    val frac = if (p != null && p.bytesTotal > 0) (p.bytesDone.toFloat() / p.bytesTotal).coerceIn(0f, 1f) else 0f
    Card(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    p?.file?.substringAfterLast('/')?.ifEmpty { null } ?: "Подготовка...",
                    maxLines = 1, overflow = TextOverflow.Ellipsis, fontSize = 12.sp,
                    modifier = Modifier.weight(1f),
                )
                TextButton(onClick = onCancel) { Text("Стоп") }
            }
            LinearProgressIndicator(progress = { frac }, modifier = Modifier.fillMaxWidth())
            Text(
                if (p == null) "Подготовка..."
                else "${humanSize(p.bytesDone)} / ${humanSize(p.bytesTotal)} - ${humanSize(p.speedBytesPerSec)}/с" +
                    if (p.filesTotal > 1) ", файл ${p.filesDone + 1}/${p.filesTotal}" else "",
                fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 2.dp),
            )
        }
    }
}

@Composable
private fun Breadcrumbs(shareName: String, currentPath: String, onJump: (String) -> Unit) {
    val segments = if (currentPath.isEmpty()) emptyList() else currentPath.split('/')
    Row(modifier = Modifier.fillMaxWidth().padding(bottom = 2.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(
            shareName, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.clickable { onJump("") }.weight(1f, fill = false),
        )
        var acc = ""
        segments.forEach { seg ->
            acc = if (acc.isEmpty()) seg else "$acc/$seg"
            val target = acc
            Text(" / ", fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(
                seg, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis,
                color = MaterialTheme.colorScheme.primary,
                modifier = Modifier.clickable { onJump(target) },
            )
        }
    }
}

@Composable
private fun SectionLabel(text: String) {
    Text(
        text, style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(top = 8.dp, bottom = 2.dp),
    )
}

// ── helpers ─────────────────────────────────────────────────────────

/** The plus control's "get it" green; same tone as the log screen's info colour. */
private val DownloadGreen = Color(0xFF4CAF50)

private fun openLocalFile(context: Context, file: File) {
    try {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
        val mime = context.contentResolver.getType(uri) ?: "*/*"
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, mime)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        context.startActivity(Intent.createChooser(intent, "Открыть файл"))
    } catch (_: Exception) {
        Toast.makeText(context, "Нет приложения, чтобы открыть этот файл", Toast.LENGTH_SHORT).show()
    }
}

private fun humanSize(bytes: Long): String = when {
    bytes >= 1L shl 30 -> "%.1f ГБ".format(bytes / (1L shl 30).toDouble())
    bytes >= 1 shl 20 -> "%.1f МБ".format(bytes / (1 shl 20).toDouble())
    bytes >= 1 shl 10 -> "%.1f КБ".format(bytes / (1 shl 10).toDouble())
    else -> "$bytes Б"
}
