package com.xrproxy.app.ui.servers

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.VisibilityOff
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.xrproxy.app.data.ServerProfile
import com.xrproxy.app.data.ServerSource
import com.xrproxy.app.ui.VpnViewModel
import java.time.OffsetDateTime
import java.util.UUID

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ServerEditScreen(
    initial: ServerProfile?,
    onSave: (ServerProfile) -> Unit,
    onCancel: () -> Unit,
) {
    val isCreate = initial == null
    val base = initial ?: ServerProfile(
        id = UUID.randomUUID().toString(),
        name = "",
        serverAddress = "",
        createdAt = OffsetDateTime.now().toString(),
        source = ServerSource.Manual,
    )

    var name by remember { mutableStateOf(base.name) }
    var address by remember { mutableStateOf(base.serverAddress) }
    var port by remember { mutableStateOf(base.serverPort.toString()) }
    var key by remember { mutableStateOf(base.obfuscationKey) }
    var showKey by remember { mutableStateOf(false) }
    var modifier by remember { mutableStateOf(base.modifier) }
    var salt by remember { mutableStateOf(base.salt.toString()) }
    var preset by remember { mutableStateOf(base.routingPreset) }
    var customDomains by remember { mutableStateOf(base.customDomains) }
    var customIpRanges by remember { mutableStateOf(base.customIpRanges) }

    var nameError by remember { mutableStateOf(false) }
    var addressError by remember { mutableStateOf(false) }
    var keyError by remember { mutableStateOf(false) }

    val clipboardManager = LocalClipboardManager.current

    fun validate(): Boolean {
        nameError = name.isBlank()
        addressError = address.isBlank()
        keyError = key.isBlank()
        return !nameError && !addressError && !keyError
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(if (isCreate) "Новый сервер" else "Изменить сервер") },
                navigationIcon = {
                    IconButton(onClick = onCancel) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back")
                    }
                },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 24.dp)
                .verticalScroll(rememberScrollState()),
        ) {
            OutlinedTextField(
                value = name, onValueChange = { name = it; nameError = false },
                label = { Text("Имя сервера") },
                placeholder = { Text("Home VPS") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                isError = nameError,
                supportingText = if (nameError) {{ Text("Обязательное поле") }} else null,
            )
            Spacer(Modifier.height(8.dp))

            OutlinedTextField(
                value = address, onValueChange = { address = it; addressError = false },
                label = { Text("Адрес сервера") },
                placeholder = { Text("1.2.3.4") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                isError = addressError,
                supportingText = if (addressError) {{ Text("Обязательное поле") }} else null,
            )
            Spacer(Modifier.height(8.dp))

            OutlinedTextField(
                value = port, onValueChange = { port = it },
                label = { Text("Порт") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            )
            Spacer(Modifier.height(16.dp))

            Text("Обфускация", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))

            OutlinedTextField(
                value = key, onValueChange = { key = it; keyError = false },
                label = { Text("Ключ (base64)") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                visualTransformation = if (showKey) VisualTransformation.None
                else PasswordVisualTransformation(),
                isError = keyError,
                supportingText = if (keyError) {{ Text("Обязательное поле") }} else null,
                trailingIcon = {
                    IconButton(onClick = { showKey = !showKey }) {
                        Icon(
                            if (showKey) Icons.Default.VisibilityOff else Icons.Default.Visibility,
                            "Toggle",
                        )
                    }
                },
            )
            Spacer(Modifier.height(8.dp))

            OutlinedTextField(
                value = salt, onValueChange = { salt = it },
                label = { Text("Salt") },
                modifier = Modifier.fillMaxWidth(), singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            )
            Spacer(Modifier.height(16.dp))

            Text("Маршрутизация", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))

            Row(modifier = Modifier.fillMaxWidth()) {
                for ((value, label) in listOf(
                    "russia" to "Russia",
                    "proxy_all" to "Proxy all",
                    "custom" to "Custom",
                )) {
                    FilterChip(
                        selected = preset == value,
                        onClick = { preset = value },
                        label = { Text(label) },
                        leadingIcon = if (preset == value) {
                            { Icon(Icons.Default.Check, null, Modifier.size(16.dp)) }
                        } else null,
                        modifier = Modifier.padding(end = 8.dp),
                    )
                }
            }

            if (preset == "custom") {
                Spacer(Modifier.height(8.dp))
                OutlinedButton(
                    onClick = {
                        val text = clipboardManager.getText()?.text ?: ""
                        if (text.isNotBlank()) {
                            val (d, r) = VpnViewModel.parseTomlDomains(text)
                            if (d.isNotEmpty() || r.isNotEmpty()) {
                                customDomains = d.joinToString("\n")
                                customIpRanges = r.joinToString("\n")
                            }
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Icon(Icons.Default.ContentPaste, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Import TOML from clipboard")
                }
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = customDomains, onValueChange = { customDomains = it },
                    label = { Text("Domains to proxy") },
                    placeholder = { Text("youtube.com\n*.google.com") },
                    modifier = Modifier.fillMaxWidth().height(120.dp), maxLines = 8,
                )
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = customIpRanges, onValueChange = { customIpRanges = it },
                    label = { Text("IP ranges to proxy") },
                    placeholder = { Text("91.108.56.0/22") },
                    modifier = Modifier.fillMaxWidth().height(100.dp), maxLines = 6,
                )
            }

            if (base.hubUrl.isNotBlank()) {
                Spacer(Modifier.height(16.dp))
                Text("Hub", style = MaterialTheme.typography.titleSmall)
                Spacer(Modifier.height(4.dp))
                Text(
                    "URL: ${base.hubUrl}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (base.hubPreset.isNotBlank()) {
                    Text(
                        "Preset: ${base.hubPreset}",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            Spacer(Modifier.height(24.dp))
            Button(
                onClick = {
                    if (!validate()) return@Button
                    val autoName = name.ifBlank { address }
                    val profile = base.copy(
                        name = autoName,
                        serverAddress = address,
                        serverPort = port.toIntOrNull() ?: 8443,
                        obfuscationKey = key,
                        modifier = modifier,
                        salt = salt.toLongOrNull() ?: 0xDEADBEEFL,
                        routingPreset = preset,
                        customDomains = if (preset == "custom") customDomains else "",
                        customIpRanges = if (preset == "custom") customIpRanges else "",
                    )
                    onSave(profile)
                },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.Check, null, Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text("Сохранить")
            }
            Spacer(Modifier.height(8.dp))
            OutlinedButton(onClick = onCancel, modifier = Modifier.fillMaxWidth()) {
                Text("Отмена")
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}
