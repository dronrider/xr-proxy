package com.xrproxy.app.ui.servers

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.data.ServerSource

@Composable
fun ServersSection(
    servers: List<ServerProfile>,
    activeId: String?,
    isConnected: Boolean,
    onSetActive: (String) -> Unit,
    onEdit: (ServerProfile) -> Unit,
    onDelete: (String) -> Unit,
    onAddServer: () -> Unit,
) {
    var deleteTarget by remember { mutableStateOf<ServerProfile?>(null) }

    Text(
        "Серверы (${servers.size})",
        style = MaterialTheme.typography.titleMedium,
        modifier = Modifier.padding(vertical = 8.dp),
    )

    for (server in servers) {
        val isActive = server.id == activeId
        ServerCard(
            server = server,
            isActive = isActive,
            showSetActive = !isActive,
            onSetActive = { onSetActive(server.id) },
            onEdit = { onEdit(server) },
            onDelete = { deleteTarget = server },
        )
        Spacer(Modifier.height(8.dp))
    }

    Spacer(Modifier.height(8.dp))
    Button(
        onClick = onAddServer,
        modifier = Modifier.fillMaxWidth(),
    ) { Text("+ Добавить сервер") }
    Spacer(Modifier.height(16.dp))

    deleteTarget?.let { target ->
        val isActiveAndConnected = target.id == activeId && isConnected
        AlertDialog(
            onDismissRequest = { deleteTarget = null },
            title = { Text("Удалить сервер") },
            text = {
                Text(
                    if (isActiveAndConnected)
                        "Соединение будет разорвано, сервер «${target.name}» будет удалён."
                    else
                        "Удалить сервер «${target.name}»?"
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    deleteTarget = null
                    onDelete(target.id)
                }) {
                    Text("Удалить", color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = {
                TextButton(onClick = { deleteTarget = null }) { Text("Отмена") }
            },
        )
    }
}

@Composable
private fun ServerCard(
    server: ServerProfile,
    isActive: Boolean,
    showSetActive: Boolean,
    onSetActive: () -> Unit,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
) {
    var menuExpanded by remember { mutableStateOf(false) }

    OutlinedCard(
        modifier = Modifier.fillMaxWidth(),
        border = if (isActive)
            BorderStroke(2.dp, MaterialTheme.colorScheme.primary)
        else
            BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
    ) {
        Row(
            modifier = Modifier.padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            RadioButton(selected = isActive, onClick = onSetActive)
            Spacer(Modifier.width(8.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(server.name, style = MaterialTheme.typography.bodyLarge)
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text(
                        server.displaySubtitle,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Text(
                        server.presetLabel,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
                val sourceLabel = when (server.source) {
                    ServerSource.Invite -> "Invite"
                    ServerSource.Manual -> "Manual"
                }
                Text(
                    sourceLabel,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            IconButton(onClick = { menuExpanded = true }) {
                Icon(Icons.Default.MoreVert, "Menu")
            }
            DropdownMenu(
                expanded = menuExpanded,
                onDismissRequest = { menuExpanded = false },
            ) {
                if (showSetActive) {
                    DropdownMenuItem(
                        text = { Text("Сделать активным") },
                        onClick = { menuExpanded = false; onSetActive() },
                    )
                }
                DropdownMenuItem(
                    text = { Text("Изменить") },
                    onClick = { menuExpanded = false; onEdit() },
                )
                DropdownMenuItem(
                    text = { Text("Удалить", color = MaterialTheme.colorScheme.error) },
                    onClick = { menuExpanded = false; onDelete() },
                )
            }
        }
    }
}
