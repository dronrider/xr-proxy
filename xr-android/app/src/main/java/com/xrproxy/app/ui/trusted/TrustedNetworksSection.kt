package com.xrproxy.app.ui.trusted

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material.icons.filled.Wifi
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp

/**
 * "Trusted networks" settings block (task 3b-2). Lists the Wi-Fi SSIDs on
 * which the app auto-pauses its tunnel (home network already behind a router),
 * with an enable toggle and a soft permission prompt — the feature degrades
 * gracefully (just never pauses) when location permission is missing.
 */
@Composable
fun TrustedNetworksSection(
    networks: List<String>,
    enabled: Boolean,
    hasPermission: Boolean,
    onToggleEnabled: (Boolean) -> Unit,
    onAdd: (String) -> Unit,
    onRemove: (String) -> Unit,
    onRequestPermission: () -> Unit,
    availableSsids: () -> List<String>,
) {
    var addDialogOpen by remember { mutableStateOf(false) }

    Row(
        modifier = Modifier.fillMaxWidth().padding(top = 24.dp, bottom = 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Доверенные сети", style = MaterialTheme.typography.titleMedium)
        Switch(checked = enabled, onCheckedChange = onToggleEnabled)
    }

    Text(
        "В этих Wi-Fi-сетях туннель автоматически встаёт на паузу — например " +
            "дома, где трафик уже идёт через роутер. При уходе из сети туннель " +
            "поднимается сам.",
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )

    if (enabled && !hasPermission) {
        Spacer(Modifier.height(12.dp))
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF2A2418)),
        ) {
            Column(modifier = Modifier.padding(16.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Default.Warning, null, tint = Color(0xFFFFA726))
                    Spacer(Modifier.width(12.dp))
                    Text(
                        "Для определения сети нужен доступ к местоположению " +
                            "(имя Wi-Fi). Без него авто-пауза не работает.",
                        color = Color(0xFFFFA726),
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Spacer(Modifier.height(8.dp))
                OutlinedButton(onClick = onRequestPermission, modifier = Modifier.fillMaxWidth()) {
                    Text("Разрешить доступ")
                }
            }
        }
    }

    Spacer(Modifier.height(12.dp))

    if (networks.isEmpty()) {
        Text(
            "Список пуст — добавьте домашнюю сеть.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    } else {
        for (ssid in networks) {
            OutlinedCard(modifier = Modifier.fillMaxWidth()) {
                Row(
                    modifier = Modifier.padding(start = 12.dp, end = 4.dp).fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        Icons.Default.Wifi, null,
                        tint = MaterialTheme.colorScheme.primary,
                    )
                    Spacer(Modifier.width(12.dp))
                    Text(
                        ssid,
                        style = MaterialTheme.typography.bodyLarge,
                        modifier = Modifier.weight(1f),
                    )
                    IconButton(onClick = { onRemove(ssid) }) {
                        Icon(Icons.Default.Close, "Удалить", tint = MaterialTheme.colorScheme.error)
                    }
                }
            }
            Spacer(Modifier.height(8.dp))
        }
    }

    Spacer(Modifier.height(8.dp))
    Button(onClick = { addDialogOpen = true }, modifier = Modifier.fillMaxWidth()) {
        Text("+ Добавить сеть")
    }
    Spacer(Modifier.height(16.dp))

    if (addDialogOpen) {
        AddTrustedNetworkDialog(
            available = availableSsids,
            alreadyAdded = networks,
            onDismiss = { addDialogOpen = false },
            onConfirm = { ssid ->
                addDialogOpen = false
                onAdd(ssid)
            },
        )
    }
}

@Composable
private fun AddTrustedNetworkDialog(
    available: () -> List<String>,
    alreadyAdded: List<String>,
    onDismiss: () -> Unit,
    onConfirm: (String) -> Unit,
) {
    // Read the scan/current-network list once when the dialog opens; exclude
    // SSIDs already in the trusted list (case-insensitive).
    val networks = remember {
        available().filterNot { cand -> alreadyAdded.any { it.equals(cand, ignoreCase = true) } }
    }
    var manual by remember { mutableStateOf("") }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Добавить доверенную сеть") },
        text = {
            Column {
                if (networks.isEmpty()) {
                    Text(
                        "Доступные сети не найдены. Нужны разрешение на " +
                            "местоположение и включённая геолокация — или введите " +
                            "имя сети вручную.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    Text(
                        "Выберите сеть поблизости:",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(8.dp))
                    Column(
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 240.dp)
                            .verticalScroll(rememberScrollState()),
                    ) {
                        for (ssid in networks) {
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .clickable { onConfirm(ssid) }
                                    .padding(vertical = 12.dp),
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Icon(
                                    Icons.Default.Wifi, null,
                                    tint = MaterialTheme.colorScheme.primary,
                                )
                                Spacer(Modifier.width(12.dp))
                                Text(ssid, style = MaterialTheme.typography.bodyLarge)
                            }
                        }
                    }
                }

                Spacer(Modifier.height(12.dp))
                Text(
                    "Или вручную (например, скрытая сеть):",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = manual,
                    onValueChange = { manual = it },
                    label = { Text("SSID") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    trailingIcon = {
                        TextButton(onClick = { onConfirm(manual) }, enabled = manual.isNotBlank()) {
                            Text("Добавить")
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {},
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Отмена") }
        },
    )
}
