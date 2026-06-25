package com.xrproxy.app.ui.files

import android.content.Intent
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.AlertDialog
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.viewmodel.compose.viewModel
import com.xrproxy.app.model.ManifestEntry
import com.xrproxy.app.model.ShareConfig

/**
 * The "Files" tab (LLD-19): list shares from the hub, configure each (token +
 * SAF folder), download files one-time, or enable background mirror sync. All
 * diff/download logic is in Rust via [FilesViewModel]; this is purely UI.
 */
@Composable
fun FilesScreen(hubUrl: String?, modifier: Modifier = Modifier) {
    val vm: FilesViewModel = viewModel()
    val ui by vm.ui.collectAsState()
    val configs by vm.configs.collectAsState()
    val context = LocalContext.current

    // Refresh hub index + run a foreground mirror when the tab opens.
    LaunchedEffect(Unit) {
        vm.refreshHub(hubUrl)
        vm.syncAllNow()
    }
    LaunchedEffect(ui.message) {
        ui.message?.let {
            Toast.makeText(context, it, Toast.LENGTH_SHORT).show()
            vm.consumeMessage()
        }
    }

    // SAF folder picker — assigns the chosen tree to the pending share.
    var pendingFolderFor by remember { mutableStateOf<String?>(null) }
    val folderPicker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree()
    ) { uri ->
        val shareId = pendingFolderFor
        pendingFolderFor = null
        if (uri != null && shareId != null) {
            context.contentResolver.takePersistableUriPermission(
                uri,
                Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
            )
            vm.setFolder(shareId, uri)
        }
    }
    val pickFolder: (String) -> Unit = { shareId ->
        pendingFolderFor = shareId
        folderPicker.launch(null)
    }

    var tokenDialogFor by remember { mutableStateOf<ShareConfig?>(null) }

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
                IconButton(onClick = { vm.refreshHub(hubUrl) }) {
                    Icon(Icons.Default.Refresh, contentDescription = "Обновить из хаба")
                }
            }
        }

        if (ui.loadingHub) {
            item { CircularProgressIndicator(modifier = Modifier.padding(8.dp)) }
        }

        // ── Shares available on the hub but not yet added ──
        if (addable.isNotEmpty()) {
            item { SectionLabel("Доступно на хабе") }
            items(addable, key = { it.shareId }) { info ->
                Card(modifier = Modifier.fillMaxWidth()) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(12.dp),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(modifier = Modifier.weight(1f)) {
                            Text(info.name, style = MaterialTheme.typography.titleMedium)
                            Text("${info.addr}:${info.port}", fontSize = 12.sp,
                                color = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                        Button(onClick = { vm.addShare(info) }) { Text("Добавить") }
                    }
                }
            }
        }

        // ── Configured shares ──
        item { SectionLabel("Мои шары") }
        if (configs.isEmpty()) {
            item {
                Text(
                    "Пока нет шар. Обновите список из хаба и добавьте нужные.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(8.dp),
                )
            }
        }
        items(configs, key = { it.shareId }) { cfg ->
            ShareCard(
                cfg = cfg,
                busy = ui.busyShareId == cfg.shareId,
                isOpen = ui.openShareId == cfg.shareId,
                onPasteToken = { tokenDialogFor = cfg },
                onPickFolder = { pickFolder(cfg.shareId) },
                onToggleSync = { vm.setSyncEnabled(cfg.shareId, it) },
                onOpen = { if (ui.openShareId == cfg.shareId) vm.closeShare() else vm.openShare(cfg) },
                onSyncNow = { vm.syncNow(cfg) },
                onRemove = { vm.removeShare(cfg.shareId) },
            )
        }

        // ── Manifest (one-time download) for the opened share ──
        val open = configs.firstOrNull { it.shareId == ui.openShareId }
        if (open != null) {
            item { SectionLabel("Файлы шары «${open.name}»") }
            if (ui.manifestLoading) {
                item { CircularProgressIndicator(modifier = Modifier.padding(8.dp)) }
            } else {
                manifestPicker(open, ui.manifest, onDownload = { entries ->
                    val tree = open.treeUri
                    if (tree == null) pickFolder(open.shareId)
                    else vm.downloadSelected(open, entries, android.net.Uri.parse(tree))
                })
            }
        }

        item { Spacer(Modifier.height(24.dp)) }
    }

    // ── Paste-token dialog ──
    tokenDialogFor?.let { cfg ->
        var text by remember { mutableStateOf(cfg.tokenJson ?: "") }
        AlertDialog(
            onDismissRequest = { tokenDialogFor = null },
            title = { Text("Токен доступа") },
            text = {
                Column {
                    Text(
                        "Вставьте токен, который выдал владелец шары (JSON из хаба).",
                        fontSize = 13.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(8.dp))
                    OutlinedTextField(
                        value = text,
                        onValueChange = { text = it },
                        modifier = Modifier.fillMaxWidth(),
                        placeholder = { Text("{\"share_id\":...,\"exp\":...,\"signature\":...}") },
                        minLines = 3,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    vm.setToken(cfg.shareId, text)
                    tokenDialogFor = null
                }) { Text("Сохранить") }
            },
            dismissButton = { TextButton(onClick = { tokenDialogFor = null }) { Text("Отмена") } },
        )
    }
}

@Composable
private fun ShareCard(
    cfg: ShareConfig,
    busy: Boolean,
    isOpen: Boolean,
    onPasteToken: () -> Unit,
    onPickFolder: () -> Unit,
    onToggleSync: (Boolean) -> Unit,
    onOpen: () -> Unit,
    onSyncNow: () -> Unit,
    onRemove: () -> Unit,
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(12.dp)) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(cfg.name, style = MaterialTheme.typography.titleMedium)
                    Text("${cfg.addr}:${cfg.port}", fontSize = 12.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                if (busy) CircularProgressIndicator(modifier = Modifier.width(22.dp).height(22.dp))
                IconButton(onClick = onRemove) {
                    Icon(Icons.Default.Delete, contentDescription = "Удалить")
                }
            }

            Spacer(Modifier.height(6.dp))
            // Status chips: token + folder.
            Text(
                buildString {
                    append(if (cfg.hasToken) "✓ токен" else "✗ нет токена")
                    append("   ")
                    append(if (cfg.treeUri != null) "✓ папка" else "✗ нет папки")
                },
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Spacer(Modifier.height(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = onPasteToken) { Text(if (cfg.hasToken) "Токен" else "Вставить токен") }
                OutlinedButton(onClick = onPickFolder) { Text(if (cfg.treeUri != null) "Папка" else "Выбрать папку") }
            }
            Spacer(Modifier.height(4.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                OutlinedButton(onClick = onOpen, enabled = cfg.hasToken) {
                    Text(if (isOpen) "Скрыть" else "Файлы")
                }
                OutlinedButton(onClick = onSyncNow, enabled = cfg.hasToken && cfg.treeUri != null) {
                    Text("Синк")
                }
                Spacer(Modifier.weight(1f))
                Text("Синк", fontSize = 12.sp)
                Switch(checked = cfg.syncEnabled, onCheckedChange = onToggleSync)
            }
        }
    }
}

/** Manifest rows with checkboxes + a download button. Returns nothing; calls
 *  [onDownload] with the selected entries. */
private fun androidx.compose.foundation.lazy.LazyListScope.manifestPicker(
    cfg: ShareConfig,
    entries: List<ManifestEntry>,
    onDownload: (List<ManifestEntry>) -> Unit,
) {
    if (entries.isEmpty()) {
        item { Text("Файлов нет", modifier = Modifier.padding(8.dp)) }
        return
    }
    item {
        val checked = remember(cfg.shareId) { mutableStateMapOf<String, Boolean>() }
        Card(modifier = Modifier.fillMaxWidth()) {
            Column(modifier = Modifier.padding(8.dp)) {
                entries.forEach { e ->
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Checkbox(
                            checked = checked[e.path] == true,
                            onCheckedChange = { checked[e.path] = it },
                        )
                        Column(modifier = Modifier.weight(1f)) {
                            Text(e.path, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            Text(humanSize(e.size), fontSize = 11.sp,
                                color = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                    }
                    HorizontalDivider()
                }
                Spacer(Modifier.height(8.dp))
                val selected = entries.filter { checked[it.path] == true }
                Button(
                    onClick = { onDownload(selected) },
                    enabled = selected.isNotEmpty(),
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Скачать выбранное (${selected.size})") }
            }
        }
    }
}

@Composable
private fun SectionLabel(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(top = 8.dp, bottom = 2.dp),
    )
}

private fun humanSize(bytes: Long): String = when {
    bytes >= 1 shl 20 -> "%.1f МБ".format(bytes / (1 shl 20).toDouble())
    bytes >= 1 shl 10 -> "%.1f КБ".format(bytes / (1 shl 10).toDouble())
    else -> "$bytes Б"
}
