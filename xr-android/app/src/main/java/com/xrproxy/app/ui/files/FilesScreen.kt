@file:OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)

package com.xrproxy.app.ui.files

import android.content.Context
import android.content.Intent
import android.os.Build
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.basicMarquee
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.AddLink
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.DriveFileMove
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import androidx.lifecycle.viewmodel.compose.viewModel
import com.xrproxy.app.data.StorageAccess
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig
import com.xrproxy.app.model.TreeNode
import com.xrproxy.app.model.explorerLevel
import java.io.File

/**
 * Files tab (LLD-19, XR-031): a list of shares ("drives") and an Explorer that
 * navigates one share's folders. Tap a file to download + open it; tick files or
 * whole folders to mirror. A compact progress bar with speed + cancel covers any
 * running transfer.
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

    if (openConfig != null) {
        ExplorerView(vm, ui, openConfig, context, modifier)
    } else {
        ShareListView(vm, ui, configs, hubUrl, inviteToken, modifier)
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

    val deleteAllCfg = configs.firstOrNull { it.shareId == ui.confirmDeleteAllFor }
    if (deleteAllCfg != null) {
        AlertDialog(
            onDismissRequest = { vm.dismissDeleteAll() },
            title = { Text("Удалить всё скачанное?") },
            text = {
                Text(
                    "Галочек нет. Синк удалит все локальные копии шары «${deleteAllCfg.name}». " +
                        "На сервере файлы останутся.",
                    fontSize = 13.sp,
                )
            },
            confirmButton = { TextButton(onClick = { vm.confirmDeleteAll() }) { Text("Удалить") } },
            dismissButton = { TextButton(onClick = { vm.dismissDeleteAll() }) { Text("Отмена") } },
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
    modifier: Modifier,
) {
    val knownIds = configs.map { it.shareId }.toSet()
    val addable = ui.hubShares.filter { it.shareId !in knownIds }

    LazyColumn(
        modifier = modifier.padding(horizontal = 16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        item {
            Row(
                modifier = Modifier.fillMaxWidth().padding(top = 12.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Файлы", style = MaterialTheme.typography.titleLarge)
                IconButton(onClick = { vm.refreshHub(hubUrl, inviteToken) }) {
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
        ui.progress?.let { p -> item { ProgressBar(p) { vm.cancelTransfer() } } }
        if (ui.loadingHub) item { CircularProgressIndicator(modifier = Modifier.padding(8.dp)) }

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
                    if (ui.hubOffline) "Сети нет, а сохранённых шар пока нет"
                    else "Пока нет шар. Обнови список и добавь нужные.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(8.dp),
                )
            }
        }
        items(configs, key = { it.shareId }) { cfg ->
            Card(modifier = Modifier.fillMaxWidth().clickable { vm.openShare(cfg) }) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(Icons.Default.Folder, contentDescription = null, modifier = Modifier.size(28.dp))
                    Spacer(Modifier.width(12.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Text(cfg.name, style = MaterialTheme.typography.titleMedium, maxLines = 1,
                            modifier = Modifier.basicMarquee())
                        Text(
                            if (cfg.selection.isEmpty()) "ничего не выбрано" else "выбрано: ${cfg.selection.size}",
                            fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        Text(
                            "Папка: ${StorageAccess.label(cfg.storagePath)}",
                            fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = 1, overflow = TextOverflow.Ellipsis,
                        )
                    }
                    IconButton(onClick = { vm.openStorageDialog(cfg.shareId) }) {
                        Icon(Icons.Default.DriveFileMove, contentDescription = "Папка хранения")
                    }
                    Switch(checked = cfg.syncEnabled, onCheckedChange = { vm.setSyncEnabled(cfg.shareId, it) })
                    IconButton(onClick = { vm.removeShare(cfg.shareId) }) {
                        Icon(Icons.Default.Delete, contentDescription = "Удалить")
                    }
                }
            }
        }
        item { Spacer(Modifier.height(24.dp)) }
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
    val level = explorerLevel(ui.manifest, ui.currentPath)

    // Resolve the selection to actual files: total selected, and how many are not
    // yet downloaded (what a sync would fetch). Folders expand to their files.
    var totalFiles = 0
    var newFiles = 0
    ui.manifest.forEach { e ->
        if (isSelected(e.path, cfg.selection)) {
            totalFiles++
            if (e.path !in ui.localPaths) newFiles++
        }
    }

    Column(modifier = modifier.fillMaxWidth().padding(horizontal = 12.dp)) {
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
            // Sync the selected subset; the icon + count light up once something is
            // ticked. The count is "<to download> / <total selected files>".
            IconButton(onClick = { vm.syncNow(cfg) }) {
                Icon(
                    Icons.Default.Sync,
                    contentDescription = "Синкать выбранное",
                    tint = if (totalFiles > 0) MaterialTheme.colorScheme.primary
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (totalFiles > 0) {
                Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(end = 4.dp)) {
                    Text("$newFiles", color = MaterialTheme.colorScheme.primary,
                        fontWeight = FontWeight.Bold, fontSize = 13.sp)
                    Text("/$totalFiles", color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 13.sp)
                }
            }
            Spacer(Modifier.width(6.dp))
            Text("Синк", fontSize = 12.sp)
            Spacer(Modifier.width(4.dp))
            Switch(checked = cfg.syncEnabled, onCheckedChange = { vm.setSyncEnabled(cfg.shareId, it) })
        }
        Breadcrumbs(cfg.name, ui.currentPath) { vm.navigateTo(it) }
        if (ui.offlineLocal && ui.manifest.isNotEmpty()) {
            Text(
                "Офлайн: показаны только скачанные файлы",
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(vertical = 2.dp),
            )
        }
        val p = ui.progress
        if (p != null) ProgressBar(p) { vm.cancelTransfer() }
        // The live import job's row (LLD-29): the agent downloads, this is just
        // the counter and the cancel; leaving the screen does not interrupt.
        val importJob = ui.importJob
        if (importJob != null && importJob.shareId == cfg.shareId) {
            ImportRow(importJob) { vm.cancelImport(cfg) }
        }
        HorizontalDivider(modifier = Modifier.padding(vertical = 4.dp))

        when {
            ui.manifestLoading -> CircularProgressIndicator(modifier = Modifier.padding(16.dp))
            ui.manifest.isEmpty() && ui.offlineLocal -> Text(
                "Сети нет, а скачанных файлов пока нет", modifier = Modifier.padding(16.dp),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            level.isEmpty() -> Text(
                "Папка пуста", modifier = Modifier.padding(16.dp),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            else -> LazyColumn {
                items(level, key = { it.path }) { node ->
                    when (node) {
                        is TreeNode.Folder -> FolderRow(node, cfg, vm)
                        is TreeNode.FileNode -> FileRow(node, cfg, ui, vm) { detailsFor = it }
                    }
                    HorizontalDivider()
                }
                item { Spacer(Modifier.height(24.dp)) }
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

@Composable
private fun FolderRow(node: TreeNode.Folder, cfg: ShareConfig, vm: FilesViewModel) {
    val coveredByParent = coveredByAncestor(node.path, cfg.selection)
    Row(
        modifier = Modifier.fillMaxWidth().clickable { vm.navigateTo(node.path) }.padding(vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Checkbox(
            checked = cfg.selection.contains(node.path) || coveredByParent,
            enabled = !coveredByParent,
            onCheckedChange = { vm.setSelected(cfg.shareId, node.path, it) },
        )
        Icon(Icons.Default.Folder, contentDescription = null, modifier = Modifier.size(24.dp))
        Spacer(Modifier.width(8.dp))
        Column(modifier = Modifier.weight(1f)) {
            Text(node.name, maxLines = 1, fontSize = 14.sp, modifier = Modifier.basicMarquee())
            Text("${node.fileCount} файл(ов)", fontSize = 10.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Text("›", fontSize = 22.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(6.dp))
    }
}

@Composable
private fun FileRow(
    node: TreeNode.FileNode,
    cfg: ShareConfig,
    ui: FilesViewModel.UiState,
    vm: FilesViewModel,
    onDetails: (ManifestEntry) -> Unit,
) {
    val downloaded = ui.localPaths.contains(node.entry.path)
    val coveredByParent = coveredByAncestor(node.entry.path, cfg.selection)
    Row(
        modifier = Modifier.fillMaxWidth()
            .combinedClickable(
                onClick = { vm.downloadAndOpen(cfg, node.entry) },
                onLongClick = { onDetails(node.entry) },
            )
            .padding(vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Checkbox(
            checked = cfg.selection.contains(node.entry.path) || coveredByParent,
            enabled = !coveredByParent,
            onCheckedChange = { vm.setSelected(cfg.shareId, node.entry.path, it) },
        )
        Column(modifier = Modifier.weight(1f).padding(start = 2.dp)) {
            Text(node.name, maxLines = 1, fontSize = 13.sp, modifier = Modifier.basicMarquee())
            Text(
                humanSize(node.entry.size) + if (downloaded) " · скачано, тап откроет" else " · тап скачает и откроет",
                fontSize = 10.sp,
                color = if (downloaded) MaterialTheme.colorScheme.primary
                else MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun ProgressBar(p: FilesViewModel.Progress, onCancel: () -> Unit) {
    val frac = if (p.bytesTotal > 0) (p.bytesDone.toFloat() / p.bytesTotal).coerceIn(0f, 1f) else 0f
    Card(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    p.file.substringAfterLast('/').ifEmpty { "Подготовка…" },
                    maxLines = 1, overflow = TextOverflow.Ellipsis, fontSize = 12.sp,
                    modifier = Modifier.weight(1f),
                )
                TextButton(onClick = onCancel) { Text("Стоп") }
            }
            LinearProgressIndicator(progress = { frac }, modifier = Modifier.fillMaxWidth())
            Text(
                "${humanSize(p.bytesDone)} / ${humanSize(p.bytesTotal)} · ${humanSize(p.speedBytesPerSec)}/с" +
                    if (p.filesTotal > 1) " · файл ${p.filesDone + 1}/${p.filesTotal}" else "",
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

private fun coveredByAncestor(path: String, selection: Set<String>): Boolean {
    var p = path
    while (true) {
        val i = p.lastIndexOf('/')
        if (i < 0) return false
        p = p.substring(0, i)
        if (selection.contains(p)) return true
    }
}

/** A file is selected if it is ticked itself or sits under a ticked folder. */
private fun isSelected(path: String, selection: Set<String>): Boolean =
    selection.contains(path) || coveredByAncestor(path, selection)

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
