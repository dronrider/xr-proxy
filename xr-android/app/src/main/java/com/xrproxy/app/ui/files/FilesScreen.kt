@file:OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)

package com.xrproxy.app.ui.files

import android.content.Context
import android.content.Intent
import android.net.Uri
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
import androidx.compose.foundation.layout.ColumnScope
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
import androidx.compose.material.icons.filled.ArrowDownward
import androidx.compose.material.icons.filled.ArrowUpward
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.CheckBox
import androidx.compose.material.icons.filled.CheckBoxOutlineBlank
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.OpenInNew
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Remove
import androidx.compose.material.icons.filled.Replay
import androidx.compose.material.icons.filled.SaveAlt
import androidx.compose.material.icons.filled.Schedule
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.LocalTextStyle
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
import androidx.compose.runtime.DisposableEffect
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
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.state.ToggleableState
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import androidx.lifecycle.viewmodel.compose.viewModel
import com.xrproxy.app.R
import com.xrproxy.app.data.StorageAccess
import com.xrproxy.app.ui.components.XrPullToRefresh
import com.xrproxy.app.model.ExplorerRow
import com.xrproxy.app.model.FileGrouping
import com.xrproxy.app.model.FileSort
import com.xrproxy.app.model.GitLogEntry
import com.xrproxy.app.model.GroupKind
import com.xrproxy.app.model.GroupTitle
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.SortOrder
import com.xrproxy.app.model.TreeNode
import com.xrproxy.app.model.explorerLevel
import com.xrproxy.app.model.explorerRows
import kotlinx.coroutines.launch
import java.io.File
import java.text.DateFormat
import java.text.SimpleDateFormat
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Date
import java.util.Locale

/**
 * Files tab (LLD-19, XR-031): a list of shares ("drives") and an Explorer that
 * navigates one share's folders. One control per file row (XR-044): the plus
 * queues a download, the running row shows progress with a cancel, the minus
 * removes the local copy, a broken download keeps its progress under a red tint
 * with a retry. The row tap only opens a downloaded file. Folders are tri-state
 * like selective sync in Drive/Dropbox. Долгое нажатие открывает экран
 * информации о файле (XR-257), и на нём же живут действия над файлом, включая
 * удаление из самой шары (XR-250), доступное с правом записи.
 * Привычки файлового менеджера (XR-251): порядок строк переключается в шапке
 * проводника, строка файла несёт дату из манифеста, открытый файл помечается
 * просмотренным, а ютуб-идентификатор в хвосте имени на экране не показывается.
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
            Toast.makeText(context, context.getString(R.string.files_storage_main_only), Toast.LENGTH_LONG)
                .show()
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
            Toast.makeText(context, context.getString(R.string.files_storage_no_access), Toast.LENGTH_LONG)
                .show()
        }
    }
    val startCustomPick: (String) -> Unit = startCustomPick@{ sid ->
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
            Toast.makeText(context, context.getString(R.string.files_storage_android11), Toast.LENGTH_LONG)
                .show()
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
    // Пока вкладка на экране, снимок передачи опрашивается и без открытой шары
    // (XR-056): фоновый синк идёт своим воркером, и общему индикатору неоткуда
    // узнать про него иначе.
    DisposableEffect(Unit) {
        vm.watchTransfers()
        onDispose { vm.unwatchTransfers() }
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
                message = context.getString(R.string.files_share_removed, cfg.name),
                actionLabel = context.getString(R.string.files_undo),
                duration = SnackbarDuration.Long,
            )
            if (result == SnackbarResult.ActionPerformed) vm.restoreShare(cfg)
        }
    }

    // Экран информации о файле (XR-257) лежит поверх проводника той же шары и
    // берёт строку из живого манифеста: удалённый из шары файл его закрывает
    // сам, а не показывает то, чего уже нет.
    val detailsEntry = ui.detailsPath?.let { p -> ui.manifest.firstOrNull { it.path == p } }
    LaunchedEffect(ui.detailsPath, detailsEntry) {
        if (ui.detailsPath != null && detailsEntry == null) vm.closeDetails()
    }
    // Экран истории (LLD-33) лежит поверх экрана информации: он отвечает тому
    // же файлу, и возврат из него возвращает к сведениям о файле.
    val historyEntry = ui.historyPath?.let { p -> ui.manifest.firstOrNull { it.path == p } }
    LaunchedEffect(ui.historyPath, historyEntry) {
        if (ui.historyPath != null && historyEntry == null) vm.closeHistory()
    }
    // Диалог правки (LLD-33) открывается из экрана информации, но держится на
    // состоянии: путь и хеш живут в UiState, а не в аргументах кнопки.
    val editEntry = ui.editPath?.let { p -> ui.manifest.firstOrNull { it.path == p } }
    LaunchedEffect(ui.editPath, editEntry) {
        if (ui.editPath != null && editEntry == null) vm.closeEditor()
    }

    Box(modifier = modifier) {
        if (openConfig != null) {
            if (historyEntry != null) {
                HistoryScreen(vm, ui, historyEntry, Modifier)
            } else if (detailsEntry != null) {
                FileInfoScreen(vm, ui, openConfig, detailsEntry, Modifier)
            } else {
                ExplorerView(vm, ui, openConfig, context, Modifier)
            }
        } else {
            ShareListView(vm, ui, configs, hubUrl, inviteToken, deleteWithUndo, Modifier)
        }
        SnackbarHost(
            snackbarHost,
            modifier = Modifier.align(Alignment.BottomCenter),
        )
    }

    if (editEntry != null && openConfig != null) {
        EditFileDialog(vm, ui, openConfig, editEntry)
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
                Toast.makeText(context, context.getString(R.string.files_copied), Toast.LENGTH_SHORT).show()
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
        title = {
            Text(
                stringResource(
                    if (promptMode) R.string.files_storage_title_prompt
                    else R.string.files_storage_title,
                ),
            )
        },
        text = {
            Column {
                if (promptMode) {
                    Text(
                        stringResource(R.string.files_storage_prompt_text, cfg.name),
                        fontSize = 13.sp,
                    )
                } else {
                    Text(
                        stringResource(
                            R.string.files_storage_current,
                            StorageAccess.label(LocalContext.current, cfg.storagePath),
                        ),
                        fontSize = 13.sp,
                    )
                    Spacer(Modifier.height(4.dp))
                    Text(
                        stringResource(R.string.files_storage_move_note),
                        fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                if (!StorageAccess.customFolderSupported()) {
                    Spacer(Modifier.height(6.dp))
                    Text(
                        stringResource(R.string.files_storage_android11_note),
                        fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = onCustom, enabled = StorageAccess.customFolderSupported()) {
                Text(stringResource(R.string.files_storage_custom))
            }
        },
        dismissButton = {
            TextButton(onClick = onAppDir) { Text(stringResource(R.string.files_storage_app_dir)) }
        },
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
                Text(stringResource(R.string.files_title), style = MaterialTheme.typography.titleLarge)
                IconButton(onClick = refreshByHand) {
                    Icon(
                        Icons.Default.Refresh,
                        contentDescription = stringResource(R.string.files_refresh_invite),
                    )
                }
            }
        }
        if (ui.hubOffline) {
            item {
                Text(
                    stringResource(R.string.files_hub_offline),
                    fontSize = 11.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        syncIndicatorOf(ui)?.let { ind -> item { SyncQueueBar(ind) { vm.cancelQueue() } } }
        if (ui.migratingShareId != null) item { ProgressBar(ui.transfer) { vm.cancelTransfer() } }

        if (addable.isNotEmpty()) {
            item { SectionLabel(stringResource(R.string.files_section_available)) }
            items(addable, key = { it.shareId }) { g ->
                Card(modifier = Modifier.fillMaxWidth()) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(12.dp),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(g.name, modifier = Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                        Button(onClick = { vm.addShare(g) }) {
                            Text(stringResource(R.string.files_add))
                        }
                    }
                }
            }
        }

        item { SectionLabel(stringResource(R.string.files_section_mine)) }
        // Until the store has loaded, an empty list means "still opening", so
        // hold the empty-state text back instead of flashing it.
        if (configs.isEmpty() && ui.storeReady) {
            item {
                Text(
                    stringResource(
                        if (ui.hubOffline) R.string.files_empty_offline
                        else R.string.files_empty,
                    ),
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
                        Icon(
                            Icons.Default.MoreVert,
                            contentDescription = stringResource(R.string.files_share_actions),
                        )
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

@Composable
private fun shareStatusText(cfg: ShareConfig): String {
    val selection = if (cfg.selection.isEmpty()) {
        stringResource(R.string.files_selection_none)
    } else {
        stringResource(R.string.files_selection_count, cfg.selection.size)
    }
    return stringResource(
        if (cfg.syncEnabled) R.string.files_share_status_on else R.string.files_share_status_off,
        selection,
    )
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
            title = stringResource(R.string.files_action_open),
            chevron = true,
            onClick = { dismissThen { vm.openShare(cfg) } },
        )
        SheetActionRow(
            icon = { Icon(Icons.Default.Sync, contentDescription = null) },
            title = stringResource(R.string.files_action_sync),
            trailing = {
                Switch(
                    checked = cfg.syncEnabled,
                    onCheckedChange = { vm.setSyncEnabled(cfg.shareId, it) },
                )
            },
        )
        SheetActionRow(
            icon = { Icon(Icons.Default.SaveAlt, contentDescription = null) },
            title = stringResource(R.string.files_action_storage),
            subtitle = StorageAccess.label(LocalContext.current, cfg.storagePath),
            chevron = true,
            onClick = { dismissThen { vm.openStorageDialog(cfg.shareId) } },
        )
        // Браузерный вход (LLD-33): вшитая страница шары у агента с историей
        // и правкой. Пункт стоит за write-грантом: ссылка несёт токен целиком,
        // и держателю read-гранта открывать в браузере нечего, кроме как
        // заново ввести токен руками на странице.
        if (cfg.canWrite && cfg.hasToken) {
            val context = LocalContext.current
            val tok = cfg.tokenJson
            SheetActionRow(
                icon = { Icon(Icons.Default.OpenInNew, contentDescription = null) },
                title = stringResource(R.string.files_action_web),
                chevron = true,
                onClick = {
                    if (tok != null) {
                        dismissThen {
                            openLink(context, shareWebUrl(cfg.agentBaseUrl, cfg.shareId, tok))
                        }
                    }
                },
            )
        }
        HorizontalDivider(modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
        SheetActionRow(
            icon = {
                Icon(Icons.Default.Delete, contentDescription = null,
                    tint = MaterialTheme.colorScheme.error)
            },
            title = stringResource(R.string.files_action_remove),
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
    // Открытие шары поднимает manifestLoading само, индикатор жеста поэтому
    // держим на локальном флаге ручных обновлений, как в списке шар (XR-232).
    var manualRefresh by remember { mutableStateOf(false) }
    LaunchedEffect(ui.manifestLoading) { if (!ui.manifestLoading) manualRefresh = false }
    val refreshByHand = { manualRefresh = true; vm.refreshManifest(cfg) }
    // Derived once per state change, not per recomposition: a big manifest
    // with a long queue would otherwise be rescanned for every visible row on
    // every 500ms progress tick.
    val level = remember(ui.manifest, ui.currentPath, ui.sortOrder) {
        explorerLevel(ui.manifest, ui.currentPath, ui.sortOrder)
    }
    // Фильтр непросмотренных (XR-256) режет только файлы: папку без
    // непросмотренного он оставляет, иначе в неё не зайти. Счётчик под шапкой
    // считает по тем же файлам открытого уровня.
    val fileCount = remember(level) { level.count { it is TreeNode.FileNode } }
    val unviewedCount = remember(level, ui.viewedPaths) {
        level.count { it is TreeNode.FileNode && it.entry.path !in ui.viewedPaths }
    }
    val shown = remember(level, ui.viewedPaths, ui.unviewedOnly) {
        if (!ui.unviewedOnly) level
        else level.filter { it !is TreeNode.FileNode || it.entry.path !in ui.viewedPaths }
    }
    // Группы (XR-258) собираются поверх отфильтрованного уровня, поэтому
    // счётчик в заголовке считает те строки, которые под ним и лежат.
    val rows = remember(shown, ui.grouping) { explorerRows(shown, ui.grouping) }
    // Формат берём системный: короткая дата в том виде, в каком её показывает
    // сам телефон.
    val dateFormat = remember(context) { android.text.format.DateFormat.getDateFormat(context) }
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
            ) { Text(stringResource(R.string.files_back)) }
            Spacer(Modifier.weight(1f))
            // URL import (LLD-29): the agent downloads the page into the open
            // folder. Shown only when the grant carries share:import.
            if (cfg.canImport) {
                IconButton(onClick = { vm.openImportDialog(cfg.shareId) }) {
                    Icon(
                        Icons.Default.AddLink,
                        contentDescription = stringResource(R.string.files_import_title),
                    )
                }
            }
            // Refresh the listing from the agent. Deliberately not the sync
            // action: the old circular-arrows button confused both meanings
            // (XR-044), downloads now go through the per-row controls.
            IconButton(onClick = refreshByHand) {
                Icon(
                    Icons.Default.Refresh,
                    contentDescription = stringResource(R.string.files_refresh_listing),
                )
            }
            Spacer(Modifier.width(6.dp))
            Text(stringResource(R.string.files_sync_switch), fontSize = 12.sp)
            Spacer(Modifier.width(4.dp))
            Switch(checked = cfg.syncEnabled, onCheckedChange = { vm.setSyncEnabled(cfg.shareId, it) })
        }
        Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Breadcrumbs(cfg.name, ui.currentPath, Modifier.weight(1f)) { vm.navigateTo(it) }
            SortButton(ui.sortOrder) { vm.setSort(it) }
            ViewMenuButton(
                unviewedOnly = ui.unviewedOnly,
                grouping = ui.grouping,
                onFilter = { vm.setUnviewedOnly(it) },
                onGroup = { vm.setGrouping(it) },
            )
        }
        // Включённый фильтр говорит о себе сам: без этой строки короткий список
        // не отличить от пропавших файлов.
        if (ui.unviewedOnly) {
            Row(
                modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)
                    .background(
                        MaterialTheme.colorScheme.primary.copy(alpha = 0.10f),
                        RoundedCornerShape(8.dp),
                    )
                    .padding(start = 10.dp, end = 4.dp, top = 3.dp, bottom = 3.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    stringResource(R.string.files_unviewed_banner, unviewedCount, fileCount),
                    fontSize = 11.sp, color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.weight(1f),
                )
                Icon(
                    Icons.Default.Close,
                    contentDescription = stringResource(R.string.files_unviewed_clear),
                    tint = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.size(24.dp)
                        .clickable { vm.setUnviewedOnly(false) }
                        .padding(5.dp),
                )
            }
        }
        if (ui.offlineLocal && ui.manifest.isNotEmpty()) {
            Text(
                // Полный кэшированный манифест показывает и не скачанные файлы,
                // так что «только скачанные» тут врало бы (XR-099).
                stringResource(
                    if (ui.offlineFullListing) R.string.files_offline_full
                    else R.string.files_offline_local,
                ),
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(vertical = 2.dp),
            )
        }
        syncIndicatorOf(ui)?.let { ind -> SyncQueueBar(ind) { vm.cancelQueue() } }
        if (ui.migratingShareId != null) ProgressBar(ui.transfer) { vm.cancelTransfer() }
        // Строка на каждый импорт этой шары (LLD-29, XR-175): агент качает по
        // очереди, здесь только счётчик и отмена; уход с экрана не прерывает.
        ui.importJobs.filter { it.shareId == cfg.shareId }.forEach { job ->
            ImportRow(job) { vm.cancelImport(cfg, job.jobId) }
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
                            stringResource(R.string.files_offline_nothing),
                            modifier = Modifier.padding(16.dp),
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
                            stringResource(R.string.files_folder_empty),
                            modifier = Modifier.padding(16.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    // Отфильтрованный дочиста уровень это не пустая папка, и
                    // говорить о нём надо иначе.
                    shown.isEmpty() -> item {
                        Text(
                            stringResource(R.string.files_no_unviewed),
                            modifier = Modifier.padding(16.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    else -> {
                        items(rows, key = { it.key }) { row ->
                            when (row) {
                                is ExplorerRow.Header ->
                                    GroupHeader(groupTitleText(row.title), row.count)
                                is ExplorerRow.Node -> {
                                    when (val node = row.node) {
                                        is TreeNode.Folder ->
                                            FolderRow(node, folderPresence[node.path], cfg, vm)
                                        is TreeNode.FileNode -> FileRow(
                                            node, cfg, ui, vm,
                                            isHead = node.entry.path == headPath,
                                            queued = node.entry.path != headPath && node.entry.path in queuedPaths,
                                            failed = failedByPath[node.entry.path],
                                            viewed = node.entry.path in ui.viewedPaths,
                                            dateFormat = dateFormat,
                                        ) { vm.openDetails(it.path) }
                                    }
                                    HorizontalDivider()
                                }
                            }
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

}

// -- Экран информации о файле (XR-257) ------------------------------

/**
 * Всё, что известно о файле, одним экраном: блок «Файл» с путём, размером,
 * датой, признаком просмотра и началом хеша, блок «Откуда файл» со страницей
 * импорта и каналом автора, и действия над файлом внизу.
 *
 * Раньше это был диалог из трёх строк по долгому тапу. Метаданные импорта
 * (XR-255) в него не влезали, а обрезанное имя ролика в заголовке диалога не
 * давало даже понять, о каком файле речь.
 *
 * Блок «Откуда файл» не прячется у файла без метаданных, а говорит, почему он
 * пуст: спрятанный блок не отличить от «экран не умеет это показывать».
 */
@Composable
private fun FileInfoScreen(
    vm: FilesViewModel,
    ui: FilesViewModel.UiState,
    cfg: ShareConfig,
    entry: ManifestEntry,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val dateFormat = remember(context) { android.text.format.DateFormat.getDateFormat(context) }
    var confirmDelete by remember { mutableStateOf(false) }

    val path = entry.path
    val downloaded = ui.localPaths.contains(path)
    val queued = ui.queue.any { it.matches(cfg.shareId, path) }
    val transferring = ui.transfer?.let { it.share == cfg.shareId && it.file == path } == true
    val viewed = path in ui.viewedPaths
    val date = entry.mtime.takeIf { it > 0 }?.let { dateFormat.format(Date(it * 1000)) }
    val meta = entry.meta

    Column(
        modifier = modifier.fillMaxSize().padding(horizontal = 12.dp)
            .verticalScroll(rememberScrollState()),
    ) {
        Row(modifier = Modifier.fillMaxWidth().padding(top = 6.dp)) {
            TextButton(
                onClick = { vm.closeDetails() },
                contentPadding = PaddingValues(horizontal = 8.dp),
            ) { Text(stringResource(R.string.files_back)) }
        }
        Column(modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp)) {
            // Имя целиком: обрезать его тут нечем и незачем, ради него на экран
            // и заходят. Ютуб-идентификатор в хвосте прячем, как в списке, но
            // полный путь ниже показывает и его.
            Text(
                displayFileName(path.substringAfterLast('/')),
                fontSize = 18.sp, fontWeight = FontWeight.SemiBold, lineHeight = 23.sp,
            )
            val stateWord = when {
                downloaded -> stringResource(R.string.files_state_downloaded)
                transferring -> stringResource(R.string.files_state_downloading)
                queued -> stringResource(R.string.files_state_queued)
                else -> null
            }
            Text(
                buildList {
                    add(humanSize(context, entry.size))
                    date?.let { add(it) }
                    stateWord?.let { add(it) }
                }.joinToString(SEP),
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 8.dp),
            )

            InfoCard(stringResource(R.string.files_info_file)) {
                InfoRow(stringResource(R.string.files_info_path), path)
                InfoRow(stringResource(R.string.files_info_size), humanSize(context, entry.size))
                if (date != null) InfoRow(stringResource(R.string.files_info_date), date)
                InfoRow(
                    stringResource(R.string.files_info_viewed),
                    stringResource(
                        if (viewed) R.string.files_viewed else R.string.files_not_viewed,
                    ),
                )
                // Офлайн-листинг собран по локальным файлам и хеша не знает
                // (XR-099), пустая строка «SHA-256: ...» врала бы.
                if (entry.sha256.isNotBlank()) {
                    InfoRow(
                        stringResource(R.string.files_info_sha256),
                        entry.sha256.take(16) + "...",
                        mono = true,
                    )
                }
            }

            InfoCard(stringResource(R.string.files_info_origin)) {
                if (meta == null) {
                    Text(
                        stringResource(R.string.files_info_origin_unknown),
                        fontSize = 12.5.sp, lineHeight = 18.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    if (meta.url.isNotBlank()) {
                        InfoRow(stringResource(R.string.files_info_page), meta.title, link = meta.url)
                    }
                    if (meta.source.isNotBlank() || meta.sourceUrl.isNotBlank()) {
                        InfoRow(
                            stringResource(R.string.files_info_channel),
                            meta.source,
                            link = meta.sourceUrl,
                        )
                    }
                    if (meta.published.isNotBlank()) {
                        InfoRow(
                            stringResource(R.string.files_info_published),
                            humanPublished(meta.published, dateFormat),
                        )
                    }
                }
            }

            // Скачивание уже идёт: кнопка это не действие, а состояние, второй
            // тап по ней всё равно ничего бы не добавил.
            Button(
                onClick = { if (downloaded) vm.openLocal(cfg, entry) else vm.enqueue(cfg, entry) },
                enabled = downloaded || !(queued || transferring),
                shape = RoundedCornerShape(26.dp),
                modifier = Modifier.fillMaxWidth().padding(top = 18.dp).height(52.dp),
            ) {
                Text(
                    stringResource(
                        when {
                            downloaded -> R.string.files_open
                            transferring -> R.string.files_downloading
                            queued -> R.string.files_in_queue
                            else -> R.string.files_download
                        },
                    ),
                    fontSize = 15.sp, fontWeight = FontWeight.SemiBold,
                )
            }
            if (downloaded) {
                TextButton(
                    onClick = { vm.removeLocal(cfg, entry) },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text(stringResource(R.string.files_remove_local)) }
            }
            // История и мелкая правка (LLD-33) стоят за share:write, как и
            // удаление: без права записи кнопок нет вовсе, а не «нажми и
            // получи отказ». Правка дополнительно только для текстовых
            // файлов, список тот же, что у страницы шары.
            if (cfg.canWrite) {
                TextButton(
                    onClick = { vm.openHistory(cfg, entry) },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text(stringResource(R.string.files_history)) }
                if (isEditableTextPath(path)) {
                    TextButton(
                        onClick = { vm.openEditor(cfg, entry) },
                        modifier = Modifier.fillMaxWidth(),
                    ) { Text(stringResource(R.string.files_edit)) }
                }
            }
            // Удаление из шары (XR-250) необратимо и видно только с правом
            // записи в токене; подтверждение с него не снимаем.
            if (cfg.canWrite) {
                TextButton(
                    onClick = { confirmDelete = true },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        stringResource(R.string.files_delete_remote),
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
            Spacer(Modifier.height(24.dp))
        }
    }

    if (confirmDelete) {
        AlertDialog(
            onDismissRequest = { confirmDelete = false },
            title = { Text(stringResource(R.string.files_delete_remote_title)) },
            text = {
                Text(
                    stringResource(
                        R.string.files_delete_remote_text,
                        path.substringAfterLast('/'),
                    ),
                    fontSize = 13.sp,
                )
            },
            confirmButton = {
                // Экран закроется сам, когда строка уйдёт из манифеста: отказ
                // агента оставляет и файл, и экран на месте.
                TextButton(onClick = {
                    confirmDelete = false
                    vm.deleteFromShare(cfg, entry)
                }) {
                    Text(
                        stringResource(R.string.files_delete),
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            },
            dismissButton = {
                TextButton(onClick = { confirmDelete = false }) {
                    Text(stringResource(R.string.files_cancel))
                }
            },
        )
    }
}

/** Экран истории коммитов одного файла (LLD-33, фаза 3): строки git-журнала
 *  агента, свежие сверху. Дифф здесь не строится: он остаётся странице шары,
 *  а экран отвечает на вопрос «кто, когда и что менял» словом коммита.
 *  Авто-коммит называет автора настройкой агента, поэтому имя человека в
 *  строке это норма, а не аномалия. */
@Composable
private fun HistoryScreen(
    vm: FilesViewModel,
    ui: FilesViewModel.UiState,
    entry: ManifestEntry,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier.fillMaxSize().padding(horizontal = 12.dp)
            .verticalScroll(rememberScrollState()),
    ) {
        Row(modifier = Modifier.fillMaxWidth().padding(top = 6.dp)) {
            TextButton(
                onClick = { vm.closeHistory() },
                contentPadding = PaddingValues(horizontal = 8.dp),
            ) { Text(stringResource(R.string.files_back)) }
        }
        Column(modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp)) {
            Text(
                displayFileName(entry.path.substringAfterLast('/')),
                fontSize = 18.sp, fontWeight = FontWeight.SemiBold, lineHeight = 23.sp,
            )
            Text(
                entry.path,
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 4.dp),
            )
            when {
                ui.historyLoading -> Box(
                    modifier = Modifier.fillMaxWidth().padding(top = 40.dp),
                    contentAlignment = Alignment.Center,
                ) { CircularProgressIndicator() }
                ui.historyError != null -> Text(
                    stringResource(R.string.files_history_failed, ui.historyError),
                    fontSize = 13.sp, lineHeight = 18.sp,
                    color = MaterialTheme.colorScheme.error,
                    modifier = Modifier.padding(top = 24.dp),
                )
                ui.history.isEmpty() -> Text(
                    stringResource(R.string.files_history_empty),
                    fontSize = 13.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(top = 24.dp),
                )
                else -> Column(modifier = Modifier.padding(top = 16.dp)) {
                    ui.history.forEachIndexed { i, row ->
                        CommitRow(row, top = i == 0)
                    }
                }
            }
            Spacer(Modifier.height(24.dp))
        }
    }
}

/** Одна строка истории: слово коммита, под ним автор, дата и короткий хеш. */
@Composable
private fun CommitRow(row: GitLogEntry, top: Boolean) {
    Column(modifier = Modifier.fillMaxWidth().padding(top = if (top) 0.dp else 14.dp)) {
        Text(row.subject, fontSize = 14.sp, lineHeight = 19.sp)
        Text(
            buildList {
                add(row.author)
                add(humanCommitDate(row.date))
                add(row.sha.take(7))
            }.joinToString(SEP),
            fontSize = 12.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(top = 3.dp),
        )
    }
}

/** Дата коммита приходит ISO-меткой с зоной; показываем её системным языком в
 *  зоне устройства. Неразобранную строку оставляем как есть: сутки в истории
 *  важнее идеального формата. */
private fun humanCommitDate(iso: String): String = runCatching {
    val fmt = DateTimeFormatter.ofPattern("d MMM yyyy, HH:mm", Locale.getDefault())
    fmt.format(OffsetDateTime.parse(iso).atZoneSameInstant(ZoneId.systemDefault()))
}.getOrDefault(iso)

/** Диалог мелкой правки (LLD-33): текст файла в многострочном поле, сохранение
 *  уезжает тем же PUT с If-Match, что и правка со страницы шары. Поле держится
 *  в состоянии вью-модели, а не в remember: поворот экрана не должен стирать
 *  набранное, а закрывается диалог только по явной кнопке. */
@Composable
private fun EditFileDialog(
    vm: FilesViewModel,
    ui: FilesViewModel.UiState,
    cfg: ShareConfig,
    entry: ManifestEntry,
) {
    AlertDialog(
        onDismissRequest = { if (!ui.editBusy) vm.closeEditor() },
        title = {
            Text(stringResource(R.string.files_edit_title, entry.path.substringAfterLast('/')))
        },
        text = {
            if (ui.editLoading) {
                Box(
                    modifier = Modifier.fillMaxWidth().height(160.dp),
                    contentAlignment = Alignment.Center,
                ) { CircularProgressIndicator() }
            } else {
                OutlinedTextField(
                    value = ui.editText,
                    onValueChange = { vm.updateEditText(it) },
                    modifier = Modifier.fillMaxWidth().height(280.dp),
                    textStyle = LocalTextStyle.current.copy(
                        fontFamily = FontFamily.Monospace, fontSize = 13.sp,
                    ),
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = !ui.editLoading && !ui.editBusy,
                onClick = { vm.saveEdit(cfg, entry) },
            ) {
                Text(
                    stringResource(
                        if (ui.editBusy) R.string.files_edit_saving else R.string.files_edit_save,
                    ),
                )
            }
        },
        dismissButton = {
            TextButton(
                enabled = !ui.editBusy,
                onClick = { vm.closeEditor() },
            ) { Text(stringResource(R.string.files_cancel)) }
        },
    )
}

/** Карточка блока экрана информации: заголовок капслоком и строки под ним. */
@Composable
private fun InfoCard(title: String, content: @Composable ColumnScope.() -> Unit) {
    Card(
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
        modifier = Modifier.fillMaxWidth().padding(top = 14.dp),
    ) {
        Column(modifier = Modifier.padding(horizontal = 16.dp, vertical = 14.dp)) {
            Text(
                title.uppercase(),
                fontSize = 11.sp, fontWeight = FontWeight.SemiBold, letterSpacing = 0.4.sp,
                color = MaterialTheme.colorScheme.primary,
                modifier = Modifier.padding(bottom = 8.dp),
            )
            content()
        }
    }
}

/**
 * Строка «ключ, значение» карточки. С [link] под значением встаёт сам адрес и
 * вся пара уводит в браузер: ради ссылки на канал экран и заводился, а
 * спрятанный за названием адрес не даёт понять, куда ведёт тап.
 */
@Composable
private fun InfoRow(
    key: String,
    value: String,
    mono: Boolean = false,
    link: String? = null,
) {
    val context = LocalContext.current
    Row(
        modifier = Modifier.fillMaxWidth()
            .let { m -> if (link.isNullOrBlank()) m else m.clickable { openLink(context, link) } }
            .padding(vertical = 4.dp),
    ) {
        // Колонка ключа шире макетной: «Опубликовано» в 74dp переносилось на
        // вторую строку и растягивало строку вдвое.
        Text(
            key, fontSize = 12.5.sp, lineHeight = 17.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.width(96.dp).padding(end = 6.dp),
        )
        Column(modifier = Modifier.weight(1f)) {
            // У ролика без заголовка в метаданных значением остаётся сам адрес,
            // и второй раз его повторять незачем.
            if (value.isNotBlank()) {
                Text(
                    value, fontSize = if (mono) 11.5.sp else 12.5.sp, lineHeight = 17.sp,
                    fontFamily = if (mono) FontFamily.Monospace else null,
                    color = if (mono) MaterialTheme.colorScheme.onSurfaceVariant
                    else MaterialTheme.colorScheme.onSurface,
                )
            }
            if (!link.isNullOrBlank()) {
                Text(
                    link, fontSize = 11.5.sp, lineHeight = 15.sp,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.padding(top = if (value.isBlank()) 0.dp else 3.dp),
                )
            }
        }
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
        title = { Text(stringResource(R.string.files_import_title)) },
        text = {
            Column {
                OutlinedTextField(
                    value = url,
                    onValueChange = { url = it },
                    label = { Text(stringResource(R.string.files_import_url)) },
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
                        label = { Text(stringResource(R.string.files_import_quality_max)) },
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = { onStart(url, height) },
                enabled = url.isNotBlank(),
            ) { Text(stringResource(R.string.files_import_start)) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.files_cancel)) }
        },
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
        title = { Text(stringResource(R.string.files_import_failed)) },
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
            }) { Text(stringResource(R.string.files_copy)) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.files_close)) }
        },
    )
}

/** The task row above the file list: "Импорт: N%" with a cancel cross. Джоба,
 *  до которой воркер агента ещё не дошёл, подписана «в очереди» (XR-175). */
@Composable
private fun ImportRow(job: FilesViewModel.ImportJob, onCancel: () -> Unit) {
    Card(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Row(
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                when {
                    job.queued -> stringResource(R.string.files_import_row_queued)
                    job.progress != null ->
                        stringResource(R.string.files_import_row_progress, job.progress.toInt())
                    else -> stringResource(R.string.files_import_row_working)
                },
                fontSize = 13.sp,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onCancel) {
                Icon(
                    Icons.Default.Close,
                    contentDescription = stringResource(R.string.files_import_cancel),
                )
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
            Text(
                pluralStringResource(R.plurals.files_folder_files, node.fileCount, node.fileCount),
                fontSize = 10.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Text(">", fontSize = 22.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(6.dp))
    }
}

/** One file row (XR-044). The trailing control always carries an action for the
 *  current state: plus = queue the download, cross = cancel it, minus = delete
 *  the local copy, replay = resume a broken download from its partial. The row
 *  tap only opens a downloaded file; progress (ours or the background mirror's,
 *  matched by share + path) is painted behind the row itself. Дату и признак
 *  просмотра (XR-251) держим слева от этих управлений: глазок идёт перед
 *  именем, дата открывает строку с размером, так что ни с прогрессом, ни с
 *  красной подсветкой ошибки они не спорят. */
@Composable
private fun FileRow(
    node: TreeNode.FileNode,
    cfg: ShareConfig,
    ui: FilesViewModel.UiState,
    vm: FilesViewModel,
    isHead: Boolean,
    queued: Boolean,
    failed: FilesViewModel.FailedDownload?,
    viewed: Boolean,
    dateFormat: DateFormat,
    onDetails: (ManifestEntry) -> Unit,
) {
    val context = LocalContext.current
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
            // Тап по скачанному открывает файл, тап по остальному ведёт на
            // экран информации (XR-257): раньше он не делал ничего, а всё, что
            // о файле известно, пряталось за долгим нажатием.
            .combinedClickable(
                onClick = { if (downloaded) vm.openLocal(cfg, node.entry) else onDetails(node.entry) },
                onLongClick = { onDetails(node.entry) },
            )
            .padding(vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // Точка у непросмотренного (XR-256): метится то, что ещё не смотрели,
        // серого глазка на просмотренном владелец не видел вовсе. Метка стоит в
        // тех же 14dp отступа колонки, поэтому имя не теряет ширины.
        val notViewedLabel = stringResource(R.string.files_not_viewed)
        Box(modifier = Modifier.width(14.dp), contentAlignment = Alignment.Center) {
            if (!viewed) {
                Box(
                    modifier = Modifier.size(7.dp)
                        .background(MaterialTheme.colorScheme.primary, CircleShape)
                        .semantics { contentDescription = notViewedLabel },
                )
            }
        }
        Column(modifier = Modifier.weight(1f)) {
            // Имя целиком в двух строках: эллипс посередине оставлял от ролика
            // начало да расширение. Высоту строки держит кнопка справа со своими
            // 48dp, и плотный межстрочный интервал укладывает колонку в этот
            // запас.
            Text(
                displayFileName(node.name), maxLines = 2, fontSize = 13.sp,
                lineHeight = 15.sp, overflow = TextOverflow.Ellipsis,
            )
            val status = when {
                downloaded ->
                    humanSize(context, node.entry.size) + SEP +
                        stringResource(R.string.files_row_downloaded)
                snap != null && snap.filesTotal == 1L -> stringResource(
                    R.string.files_row_transfer,
                    humanSize(context, snap.bytesDone),
                    humanSize(context, node.entry.size),
                    humanSize(context, snap.speedBytesPerSec),
                )
                bgFetch -> stringResource(R.string.files_row_background)
                isHead -> stringResource(R.string.files_row_preparing)
                queued ->
                    humanSize(context, node.entry.size) + SEP +
                        stringResource(R.string.files_state_queued)
                failed != null -> stringResource(
                    R.string.files_row_broken,
                    humanSize(context, failed.bytesDone),
                    humanSize(context, failed.bytesTotal),
                )
                else -> humanSize(context, node.entry.size)
            }
            // Дата это mtime из манифеста, то есть когда файл появился у агента.
            // Нулевой mtime бывает у записи без даты, тогда строка остаётся
            // прежней, без пустого разделителя.
            val date = node.entry.mtime.takeIf { it > 0 }?.let { dateFormat.format(Date(it * 1000)) }
            Text(
                if (date == null) status else "$date$SEP$status",
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
                Icon(
                    Icons.Default.Remove,
                    contentDescription = stringResource(R.string.files_remove_local),
                )
            }
            isHead || bgFetch -> IconButton(onClick = { vm.cancelDownload(cfg.shareId, path) }) {
                Icon(
                    Icons.Default.Close,
                    contentDescription = stringResource(R.string.files_cancel_download),
                )
            }
            queued -> IconButton(onClick = { vm.cancelDownload(cfg.shareId, path) }) {
                Icon(
                    Icons.Default.Schedule,
                    contentDescription = stringResource(R.string.files_dequeue),
                )
            }
            failed != null -> IconButton(onClick = { vm.enqueue(cfg, node.entry) }) {
                Icon(
                    Icons.Default.Replay,
                    contentDescription = stringResource(R.string.files_resume),
                    tint = errorColor,
                )
            }
            else -> IconButton(onClick = { vm.enqueue(cfg, node.entry) }) {
                Icon(
                    Icons.Default.Add,
                    contentDescription = stringResource(R.string.files_download),
                    tint = DownloadGreen,
                )
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

/** Состояние экрана в терминах общего индикатора (XR-056). Байты текущего
 *  файла идут в расчёт только у передачи про один файл: проход зеркала по
 *  нескольким файлам отдаёт в снимке агрегатные байты, и к текущему файлу они
 *  отношения не имеют. */
private fun syncIndicatorOf(ui: FilesViewModel.UiState): SyncIndicator? {
    val t = ui.transfer
    return syncIndicator(
        SyncProgressInput(
            queueSize = ui.queue.size,
            queueDone = ui.queueDone,
            queueHeadFile = ui.queue.firstOrNull()?.entry?.path,
            nativeFile = t?.file?.ifEmpty { null },
            nativeFilesDone = t?.filesDone ?: 0L,
            nativeFilesTotal = t?.filesTotal ?: 0L,
            nativeFileFraction = if (t != null && t.filesTotal == 1L && t.bytesTotal > 0) {
                t.bytesDone.toFloat() / t.bytesTotal
            } else {
                0f
            },
            migrating = ui.migratingShareId != null,
        ),
    )
}

/** Общий индикатор очереди синка (XR-056): одна строка с именем файла,
 *  счётчиком «X из N» и стопом всей очереди, под нею тонкая полоса по батчу.
 *  Байты и скорость сюда не вернутся, они живут на строке файла (XR-044), а
 *  карточка во всю ширину под один файл занимала пол-экрана. Индикатор один и
 *  тот же на списке шар и в проводнике: очередь общая на все шары. */
@Composable
private fun SyncQueueBar(ind: SyncIndicator, onStop: () -> Unit) {
    Column(modifier = Modifier.fillMaxWidth()) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                ind.file, fontSize = 12.sp, maxLines = 1,
                overflow = TextOverflow.MiddleEllipsis, modifier = Modifier.weight(1f),
            )
            Spacer(Modifier.width(8.dp))
            Text(
                stringResource(R.string.files_sync_counter, ind.current, ind.total),
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            TextButton(onClick = onStop, contentPadding = PaddingValues(horizontal = 8.dp)) {
                Text(stringResource(R.string.files_stop), fontSize = 12.sp)
            }
        }
        LinearProgressIndicator(
            progress = { ind.fraction },
            modifier = Modifier.fillMaxWidth().height(3.dp),
        )
    }
}

/** The storage-migration card. [p] null means the native side has not flipped
 *  active yet (still listing files), rendered as an indeterminate start. */
@Composable
private fun ProgressBar(p: FilesViewModel.Progress?, onCancel: () -> Unit) {
    val context = LocalContext.current
    val frac = if (p != null && p.bytesTotal > 0) (p.bytesDone.toFloat() / p.bytesTotal).coerceIn(0f, 1f) else 0f
    Card(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    p?.file?.substringAfterLast('/')?.ifEmpty { null }
                        ?: stringResource(R.string.files_preparing),
                    maxLines = 1, overflow = TextOverflow.Ellipsis, fontSize = 12.sp,
                    modifier = Modifier.weight(1f),
                )
                TextButton(onClick = onCancel) { Text(stringResource(R.string.files_stop)) }
            }
            LinearProgressIndicator(progress = { frac }, modifier = Modifier.fillMaxWidth())
            Text(
                if (p == null) {
                    stringResource(R.string.files_preparing)
                } else {
                    stringResource(
                        R.string.files_migrate_progress,
                        humanSize(context, p.bytesDone),
                        humanSize(context, p.bytesTotal),
                        humanSize(context, p.speedBytesPerSec),
                    ) + if (p.filesTotal > 1) {
                        stringResource(
                            R.string.files_migrate_file_of,
                            p.filesDone + 1,
                            p.filesTotal,
                        )
                    } else {
                        ""
                    }
                },
                fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 2.dp),
            )
        }
    }
}

/**
 * Переключатель порядка строк (XR-251). В шапке проводника видно, чем список
 * отсортирован и в какую сторону; меню меняет поле, а повторный выбор того же
 * разворачивает направление, как это делают файловые менеджеры. Направление
 * названо и словом в contentDescription, иначе стрелку не прочитать ни
 * скринридеру, ни экранному сценарию.
 */
@Composable
private fun SortButton(order: SortOrder, onPick: (FileSort) -> Unit) {
    var menuOpen by remember { mutableStateOf(false) }
    val arrow = if (order.descending) Icons.Default.ArrowDownward else Icons.Default.ArrowUpward
    val direction = stringResource(
        if (order.descending) R.string.files_sort_desc else R.string.files_sort_asc,
    )
    Box {
        TextButton(onClick = { menuOpen = true }, contentPadding = PaddingValues(horizontal = 8.dp)) {
            Text(stringResource(sortLabel(order.mode)), fontSize = 13.sp)
            Spacer(Modifier.width(2.dp))
            Icon(arrow, contentDescription = direction, modifier = Modifier.size(14.dp))
        }
        DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
            FileSort.entries.forEach { mode ->
                DropdownMenuItem(
                    text = { Text(stringResource(sortMenuLabel(mode))) },
                    trailingIcon = {
                        if (mode == order.mode) Icon(arrow, contentDescription = direction)
                    },
                    onClick = { menuOpen = false; onPick(mode) },
                )
            }
        }
    }
}

/**
 * Меню вида (XR-256, XR-258): группировка списка и фильтр непросмотренных в
 * одном месте. Кнопкой в шапке они не стали намеренно, иначе шапка проводника
 * превратилась бы в полосу кнопок. Собранный не как обычно список виден по
 * цвету иконки, и сказано об этом там же, где эти режимы переключают.
 */
@Composable
private fun ViewMenuButton(
    unviewedOnly: Boolean,
    grouping: FileGrouping,
    onFilter: (Boolean) -> Unit,
    onGroup: (FileGrouping) -> Unit,
) {
    var menuOpen by remember { mutableStateOf(false) }
    val grouped = grouping != FileGrouping.NONE
    Box {
        IconButton(onClick = { menuOpen = true }) {
            Icon(
                Icons.Default.Tune,
                // Состояние названо словами: по цвету иконки экранный сценарий
                // и скринридер режим не прочитают.
                contentDescription = stringResource(
                    when {
                        grouped && unviewedOnly -> R.string.files_view_both_on
                        grouped -> R.string.files_view_grouping_on
                        unviewedOnly -> R.string.files_view_filter_on
                        else -> R.string.files_view
                    },
                ),
                tint = if (grouped || unviewedOnly) MaterialTheme.colorScheme.primary
                else MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
            Text(
                stringResource(R.string.files_grouping),
                fontSize = 11.sp, letterSpacing = 0.4.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 16.dp, top = 8.dp, bottom = 2.dp),
            )
            FileGrouping.entries.forEach { mode ->
                DropdownMenuItem(
                    text = { Text(stringResource(groupingLabel(mode))) },
                    leadingIcon = {
                        // Пустое место под галочкой держим всегда, иначе
                        // невыбранные пункты разъезжались бы влево.
                        if (mode == grouping) {
                            Icon(
                                Icons.Default.Check,
                                contentDescription = stringResource(R.string.files_chosen),
                                tint = MaterialTheme.colorScheme.primary,
                            )
                        } else {
                            Spacer(Modifier.size(24.dp))
                        }
                    },
                    onClick = { menuOpen = false; onGroup(mode) },
                )
            }
            HorizontalDivider()
            DropdownMenuItem(
                text = { Text(stringResource(R.string.files_view_unviewed_only)) },
                leadingIcon = {
                    Icon(
                        if (unviewedOnly) Icons.Default.CheckBox else Icons.Default.CheckBoxOutlineBlank,
                        contentDescription = null,
                        tint = if (unviewedOnly) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                },
                onClick = { menuOpen = false; onFilter(!unviewedOnly) },
            )
        }
    }
}

private fun groupingLabel(mode: FileGrouping): Int = when (mode) {
    FileGrouping.NONE -> R.string.files_group_none
    FileGrouping.DATE -> R.string.files_group_by_date
    FileGrouping.SOURCE -> R.string.files_group_by_source
}

/**
 * Подпись заголовка группы (XR-092): раскладка проводника несёт ключ, а ресурс
 * под него подбирается здесь. `when` по enum держит полноту сам: заведут новую
 * группу, и без её подписи файл не соберётся.
 */
@Composable
private fun groupTitleText(title: GroupTitle): String = when (title) {
    is GroupTitle.Text -> title.text
    is GroupTitle.Known -> stringResource(
        when (title.kind) {
            GroupKind.FOLDERS -> R.string.files_group_folders
            GroupKind.NO_SOURCE -> R.string.files_group_no_source
            GroupKind.NO_DATE -> R.string.files_group_no_date
            GroupKind.TODAY -> R.string.files_group_today
            GroupKind.THIS_WEEK -> R.string.files_group_this_week
        },
    )
}

/**
 * Текст ошибки шары по её варианту (XR-092): разбор категории живёт в
 * [ShareErrorText.kt] и об Android не знает, а ресурс к варианту подбирается
 * здесь. Берёт [Context], а не composable-окружение: этими же словами
 * [FilesViewModel] подписывает свои тосты.
 */
internal fun renderShareError(e: ShareErrorText, context: Context): String = when (e) {
    is ShareErrorText.Raw -> e.text
    is ShareErrorText.Known -> when (e.kind) {
        ShareErrorKind.AGENT_OFFLINE -> context.getString(R.string.share_error_agent_offline)
        ShareErrorKind.ACCESS_EXPIRED -> context.getString(R.string.share_error_access_expired)
        ShareErrorKind.STALE_TOKEN -> context.getString(R.string.share_error_stale_token)
        ShareErrorKind.INVITE_GONE -> context.getString(R.string.share_error_invite_gone)
        ShareErrorKind.NETWORK -> context.getString(R.string.share_error_network)
        ShareErrorKind.NOT_FOUND -> context.getString(R.string.share_error_not_found)
        ShareErrorKind.SERVER_ERROR -> context.getString(R.string.share_error_server, e.arg)
        ShareErrorKind.HTTP_STATUS -> context.getString(R.string.share_error_http, e.arg)
        ShareErrorKind.PARSE -> context.getString(R.string.share_error_parse)
        ShareErrorKind.READ -> context.getString(R.string.share_error_read)
        ShareErrorKind.MANIFEST_UNSIGNED -> context.getString(R.string.share_error_manifest_unsigned)
        ShareErrorKind.MANIFEST_SIGNATURE -> context.getString(R.string.share_error_manifest_signature)
        ShareErrorKind.IMPORT_QUEUE_FULL -> context.getString(R.string.share_error_queue_full)
        ShareErrorKind.UNKNOWN -> context.getString(R.string.share_error_unknown)
    }
}

/**
 * Заголовок группы (XR-258). Счётчик рядом с названием отвечает на первый же
 * вопрос к сгруппированному списку: сколько тут всего, ведь под заголовком
 * видно две строки из двенадцати. Считает он показанные строки, поэтому с
 * включённым фильтром говорит про непросмотренные.
 */
@Composable
private fun GroupHeader(title: String, count: Int) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(start = 14.dp, top = 12.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            title.uppercase(), fontSize = 11.sp, letterSpacing = 0.4.sp,
            fontWeight = FontWeight.SemiBold, color = MaterialTheme.colorScheme.primary,
            maxLines = 1, overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f, fill = false),
        )
        Spacer(Modifier.width(6.dp))
        Text(
            "$count", fontSize = 10.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

private fun sortLabel(mode: FileSort): Int = when (mode) {
    FileSort.NAME -> R.string.files_sort_name
    FileSort.DATE -> R.string.files_sort_date
}

private fun sortMenuLabel(mode: FileSort): Int = when (mode) {
    FileSort.NAME -> R.string.files_sort_by_name
    FileSort.DATE -> R.string.files_sort_by_date
}

@Composable
private fun Breadcrumbs(
    shareName: String,
    currentPath: String,
    modifier: Modifier = Modifier,
    onJump: (String) -> Unit,
) {
    val segments = if (currentPath.isEmpty()) emptyList() else currentPath.split('/')
    Row(modifier = modifier.padding(bottom = 2.dp), verticalAlignment = Alignment.CenterVertically) {
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

/** Разделитель кусков строки статуса: средняя точка, как в карточке шары.
 *  Задана escape-последовательностью, вне раскладок en/ru её в исходник не
 *  пускают правила проекта. */
private const val SEP = " \u00B7 "

private fun openLocalFile(context: Context, file: File) {
    try {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
        val mime = context.contentResolver.getType(uri) ?: "*/*"
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, mime)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        context.startActivity(
            Intent.createChooser(intent, context.getString(R.string.files_open_with)),
        )
    } catch (_: Exception) {
        Toast.makeText(context, context.getString(R.string.files_no_opener), Toast.LENGTH_SHORT)
            .show()
    }
}

/** Ссылка с экрана информации о файле (XR-257): страница ролика и канал автора
 *  уходят в браузер. Обработчика может не найтись только на устройстве без
 *  браузера вовсе, но падать из-за этого экран не должен. */
private fun openLink(context: Context, url: String) {
    try {
        context.startActivity(
            Intent(Intent.ACTION_VIEW, Uri.parse(url)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        )
    } catch (_: Exception) {
        Toast.makeText(context, context.getString(R.string.files_no_browser), Toast.LENGTH_SHORT)
            .show()
    }
}

/** Дата публикации приезжает от агента как «ГГГГ-ММ-ДД». Показываем её тем же
 *  системным форматом, что и дату файла, иначе на одном экране оказались бы два
 *  разных формата даты. Неразобранную строку показываем как есть. */
private fun humanPublished(published: String, dateFormat: DateFormat): String =
    runCatching {
        val iso = SimpleDateFormat("yyyy-MM-dd", Locale.US).apply { isLenient = false }
        dateFormat.format(iso.parse(published)!!)
    }.getOrDefault(published)

/** Хвост, который импорт с ютуба дописывает к имени: идентификатор ролика в
 *  квадратных скобках, ровно 11 символов алфавита base64url. Правило узкое
 *  намеренно, иначе под нож ушли бы обычные «[2024]» и «[rus]». */
private val YOUTUBE_ID_SUFFIX = Regex("""\s*\[[A-Za-z0-9_-]{11}]$""")

/** Имя файла для показа (XR-251). В узкой строке эллипсис в середине оставляет
 *  начало и хвост, и от ютуб-импорта на экране остаётся идентификатор вместо
 *  названия. Настоящее имя это ключ строки, путь скачивания и то, что уходит в
 *  JNI, поэтому режем только здесь, на отрисовке. */
private fun displayFileName(name: String): String {
    val dot = name.lastIndexOf('.')
    val stem = if (dot > 0) name.substring(0, dot) else name
    val ext = if (dot > 0) name.substring(dot) else ""
    val trimmed = stem.replace(YOUTUBE_ID_SUFFIX, "")
    // Имя из одного идентификатора («[dQw4w9WgXcQ].mp4») оставляем как есть:
    // пустая строка в списке хуже лишних скобок.
    return if (trimmed.isBlank()) name else trimmed + ext
}

/** Размер с единицей измерения. Единицу берём из ресурсов, поэтому нужен
 *  [Context]: функцию зовут и из строки файла, и с экрана информации, и из
 *  карточки переноса. */
private fun humanSize(context: Context, bytes: Long): String = when {
    bytes >= 1L shl 30 -> context.getString(R.string.files_size_gb, bytes / (1L shl 30).toDouble())
    bytes >= 1 shl 20 -> context.getString(R.string.files_size_mb, bytes / (1 shl 20).toDouble())
    bytes >= 1 shl 10 -> context.getString(R.string.files_size_kb, bytes / (1 shl 10).toDouble())
    else -> context.getString(R.string.files_size_b, bytes)
}
